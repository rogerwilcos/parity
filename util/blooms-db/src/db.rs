// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::{io, fmt};
use std::path::Path;

use ethbloom;

use file::{File, FileIterator};

/// Bloom positions in database files.
#[derive(Debug)]
struct Positions {
	top: u64,
	mid: u64,
	bot: u64
}

impl Positions {
	fn from_index(index: u64) -> Self {
		Positions {
			top: index >> 8,
			mid: index >> 4,
			bot: index,
		}
	}
}

/// Blooms database.
pub struct Database {
	/// Top level bloom file
	///
	/// Every bloom represents 16 blooms on mid level
	top: File,
	/// Mid level bloom file
	///
	/// Every bloom represents 16 blooms on bot level
	mid: File,
	/// Bot level bloom file
	///
	/// Every bloom is an ethereum header bloom
	bot: File,
}

impl Database {
	/// Opens blooms database.
	pub fn open<P>(path: P) -> io::Result<Database> where P: AsRef<Path> {
		let path = path.as_ref();
		let database = Database {
			top: File::open(path.join("top.bdb"))?,
			mid: File::open(path.join("mid.bdb"))?,
			bot: File::open(path.join("bot.bdb"))?,
		};

		Ok(database)
	}

	/// Insert consecutive blooms into database starting with positon from.
	pub fn insert_blooms<'a, I, B>(&mut self, from: u64, blooms: I) -> io::Result<()>
	where ethbloom::BloomRef<'a>: From<B>, I: Iterator<Item = B> {
		for (index, bloom) in (from..).into_iter().zip(blooms.map(Into::into)) {
			let pos = Positions::from_index(index);

			// constant forks make lead to increased ration of false positives in bloom filters
			// since we do not rebuild top or mid level, but we should not be worried about that
			// most of the time events at block n(a) occur also on block n(b) or n+1(b)
			self.top.accrue_bloom::<ethbloom::BloomRef>(pos.top, bloom)?;
			self.mid.accrue_bloom::<ethbloom::BloomRef>(pos.mid, bloom)?;
			self.bot.replace_bloom::<ethbloom::BloomRef>(pos.bot, bloom)?;
		}
		self.top.flush()?;
		self.mid.flush()?;
		self.bot.flush()
	}

	/// Returns an iterator yielding all indexes containing given bloom.
	pub fn iterate_matching<'a, 'b, B, I, II>(&'a mut self, from: u64, to: u64, blooms: II) -> io::Result<DatabaseIterator<'a, II>>
	where ethbloom::BloomRef<'b>: From<B>, 'b: 'a, II: IntoIterator<Item = B, IntoIter = I> + Copy, I: Iterator<Item = B> {
		let index = from / 256 * 256;
		let pos = Positions::from_index(index);

		let iter = DatabaseIterator {
			top: self.top.iterator_from(pos.top)?,
			mid: self.mid.iterator_from(pos.mid)?,
			bot: self.bot.iterator_from(pos.bot)?,
			state: IteratorState::Top,
			from,
			to,
			index,
			blooms,
		};

		Ok(iter)
	}
}

fn contains_any<'a, I, B>(bloom: ethbloom::Bloom, mut iterator: I) -> bool
where ethbloom::BloomRef<'a>: From<B>, I: Iterator<Item = B> {
	iterator.any(|item| bloom.contains_bloom(item))
}

/// Blooms database iterator
pub struct DatabaseIterator<'a, I> {
	top: FileIterator<'a>,
	mid: FileIterator<'a>,
	bot: FileIterator<'a>,
	state: IteratorState,
	from: u64,
	to: u64,
	index: u64,
	blooms: I,
}

impl<'a, I> fmt::Debug for DatabaseIterator<'a, I> {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		f.debug_struct("DatabaseIterator")
			.field("state", &self.state)
			.field("from", &self.from)
			.field("to", &self.to)
			.field("index", &self.index)
			.field("blooms", &"...")
			.field("top", &"...")
			.field("mid", &"...")
			.field("bot", &"...")
			.finish()
	}
}

/// Database iterator state.
#[derive(Debug)]
enum IteratorState {
	/// Iterator should read top level bloom
	Top,
	/// Iterator should read mid level bloom `x` more times
	Mid(usize),
	/// Iterator should read mid level bloom `mid` more times
	/// and bot level `mix * 16 + bot` times
	Bot { mid: usize, bot: usize },
}

impl<'a, 'b, B, I, II> Iterator for DatabaseIterator<'a, II>
where ethbloom::BloomRef<'b>: From<B>, 'b: 'a, II: IntoIterator<Item = B, IntoIter = I> + Copy, I: Iterator<Item = B> {
	type Item = io::Result<u64>;

	fn next(&mut self) -> Option<Self::Item> {
		macro_rules! try_o {
			($expr: expr) => {
				match $expr {
					Err(err) => return Some(Err(err)),
					Ok(ok) => ok,
				}
			}
		}

		macro_rules! next_bloom {
			($iter: expr) => {
				try_o!($iter.next()?)
			}
		}

		loop {
			if self.index > self.to {
				return None;
			}

			self.state = match self.state {
				IteratorState::Top => {
					if contains_any(next_bloom!(self.top), self.blooms.into_iter()) {
						IteratorState::Mid(16)
					} else {
						self.index += 256;
						try_o!(self.mid.advance(16));
						try_o!(self.bot.advance(256));
						IteratorState::Top
					}
				},
				IteratorState::Mid(left) => {
					if left == 0 {
						IteratorState::Top
					} else if contains_any(next_bloom!(self.mid), self.blooms.into_iter()) && self.index + 16 >= self.from {
						IteratorState::Bot { mid: left - 1, bot: 16 }
					} else {
						self.index += 16;
						try_o!(self.bot.advance(16));
						IteratorState::Mid(left - 1)
					}
				},
				IteratorState::Bot { mid, bot } => {
					if bot == 0 {
						IteratorState::Mid(mid)
					} else if contains_any(next_bloom!(self.bot), self.blooms.into_iter()) && self.index >= self.from {
						let result = self.index;
						self.index += 1;
						self.state = IteratorState::Bot { mid, bot: bot - 1 };
						return Some(Ok(result));
					} else {
						self.index += 1;
						IteratorState::Bot { mid, bot: bot - 1 }
					}
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use ethbloom::Bloom;
	use tempdir::TempDir;
	use super::Database;

	#[test]
	fn test_database() {
		let tempdir = TempDir::new("").unwrap();
		let mut database = Database::open(tempdir.path()).unwrap();
		database.insert_blooms(0, vec![Bloom::from(0), Bloom::from(0x01), Bloom::from(0x10), Bloom::from(0x11)].iter()).unwrap();

		let matches = database.iterate_matching(0, 3, Some(&Bloom::from(0))).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
		assert_eq!(matches, vec![0, 1, 2, 3]);

		let matches = database.iterate_matching(0, 4, Some(&Bloom::from(0))).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
		assert_eq!(matches, vec![0, 1, 2, 3]);

		let matches = database.iterate_matching(1, 3, Some(&Bloom::from(0))).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
		assert_eq!(matches, vec![1, 2, 3]);

		let matches = database.iterate_matching(1, 2, Some(&Bloom::from(0))).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
		assert_eq!(matches, vec![1, 2]);

		let matches = database.iterate_matching(0, 3, Some(&Bloom::from(0x01))).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
		assert_eq!(matches, vec![1, 3]);

		let matches = database.iterate_matching(0, 3, Some(&Bloom::from(0x10))).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
		assert_eq!(matches, vec![2, 3]);

		let matches = database.iterate_matching(2, 2, Some(&Bloom::from(0x10))).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
		assert_eq!(matches, vec![2]);
	}

	#[test]
	fn test_database2() {
		let tempdir = TempDir::new("").unwrap();
		let mut database = Database::open(tempdir.path()).unwrap();
		database.insert_blooms(254, vec![Bloom::from(0x100), Bloom::from(0x01), Bloom::from(0x10), Bloom::from(0x11)].iter()).unwrap();

		let matches = database.iterate_matching(0, 257, Some(&Bloom::from(0x01))).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
		assert_eq!(matches, vec![255, 257]);

		let matches = database.iterate_matching(0, 258, Some(&Bloom::from(0x100))).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
		assert_eq!(matches, vec![254]);

		let matches = database.iterate_matching(0, 256, Some(&Bloom::from(0x01))).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
		assert_eq!(matches, vec![255]);

		let matches = database.iterate_matching(255, 255, Some(&Bloom::from(0x01))).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
		assert_eq!(matches, vec![255]);

		let matches = database.iterate_matching(256, 256, Some(&Bloom::from(0x10))).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
		assert_eq!(matches, vec![256]);

		let matches = database.iterate_matching(256, 257, Some(&Bloom::from(0x10))).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
		assert_eq!(matches, vec![256, 257]);
	}
}