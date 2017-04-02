use std::cmp;
use std::fs::File;
use std::io::{self, BufRead, Seek};
use std::mem;
use std::path::Path;
use std::result;

use bytecount;
use csv_core::{Reader as CoreReader, ReaderBuilder as CoreReaderBuilder};

use byte_record::{self, ByteRecord};
use string_record::{self, StringRecord};
use {Error, Result, Terminator, Utf8Error};

/// Builds a CSV reader with various configuration knobs.
///
/// This builder can be used to tweak the field delimiter, record terminator
/// and more for parsing CSV. Once a CSV `Reader` is built, its configuration
/// cannot be changed.
#[derive(Debug)]
pub struct ReaderBuilder {
    builder: CoreReaderBuilder,
    capacity: usize,
    flexible: bool,
    has_headers: bool,
}

impl Default for ReaderBuilder {
    fn default() -> ReaderBuilder {
        ReaderBuilder {
            builder: CoreReaderBuilder::default(),
            capacity: 8 * (1<<10),
            flexible: false,
            has_headers: true,
        }
    }
}

impl ReaderBuilder {
    /// Create a new builder for configuring CSV parsing.
    ///
    /// To convert a builder into a reader, call one of the methods starting
    /// with `from_`.
    pub fn new() -> ReaderBuilder {
        ReaderBuilder::default()
    }

    /// Build a CSV parser from this configuration that reads data from the
    /// given file path.
    ///
    /// If there was a problem open the file at the given path, then this
    /// returns the corresponding error.
    pub fn from_path<P: AsRef<Path>>(&self, path: P) -> Result<Reader<File>> {
        Ok(Reader::new(self, File::open(path)?))
    }

    /// Build a CSV parser from this configuration that reads data from `rdr.
    ///
    /// Note that the CSV reader is buffered automatically, so you should not
    /// wrap `rdr` in a buffered reader like `io::BufReader`.
    pub fn from_reader<R: io::Read>(&self, rdr: R) -> Reader<R> {
        Reader::new(self, rdr)
    }

    /// The field delimiter to use when parsing CSV.
    ///
    /// The default is `b','`.
    pub fn delimiter(&mut self, delimiter: u8) -> &mut ReaderBuilder {
        self.builder.delimiter(delimiter);
        self
    }

    /// Whether to treat the first row as a special header row.
    ///
    /// By default, the first row is treated as a special header row, which
    /// means the header is never returned by any of the record reading methods
    /// or iterators. When this is disabled (`yes` set to `false`), the first
    /// row is not treated specially.
    ///
    /// Note that the `headers` and `byte_headers` methods are unaffected by
    /// whether this is set. Those methods always return the first record.
    pub fn has_headers(&mut self, yes: bool) -> &mut ReaderBuilder {
        self.has_headers = yes;
        self
    }

    /// Whether the number of fields in records is allowed to change or not.
    ///
    /// When disabled (which is the default), parsing CSV data will return an
    /// error if a record is found with a number of fields different from the
    /// number of fields in a previous record.
    ///
    /// When enabled, this error checking is turned off.
    pub fn flexible(&mut self, yes: bool) -> &mut ReaderBuilder {
        self.flexible = yes;
        self
    }

    /// The record terminator to use when parsing CSV.
    ///
    /// A record terminator can be any single byte. The default is a special
    /// value, `Terminator::CRLF`, which treats any occurrence of `\r`, `\n`
    /// or `\r\n` as a single record terminator.
    pub fn terminator(
        &mut self,
        term: Terminator,
    ) -> &mut ReaderBuilder {
        self.builder.terminator(term);
        self
    }

    /// The quote character to use when parsing CSV.
    ///
    /// The default is `b'"'`.
    pub fn quote(&mut self, quote: u8) -> &mut ReaderBuilder {
        self.builder.quote(quote);
        self
    }

    /// The escape character to use when parsing CSV.
    ///
    /// In some variants of CSV, quotes are escaped using a special escape
    /// character like `\` (instead of escaping quotes by doubling them).
    ///
    /// By default, recognizing these idiosyncratic escapes is disabled.
    pub fn escape(&mut self, escape: Option<u8>) -> &mut ReaderBuilder {
        self.builder.escape(escape);
        self
    }

    /// Enable double quote escapes.
    ///
    /// This is enabled by default, but it may be disabled. When disabled,
    /// doubled quotes are not interpreted as escapes.
    pub fn double_quote(&mut self, yes: bool) -> &mut ReaderBuilder {
        self.builder.double_quote(yes);
        self
    }

    /// A convenience method for specifying a configuration to read ASCII
    /// delimited text.
    ///
    /// This sets the delimiter and record terminator to the ASCII unit
    /// separator (`\x1F`) and record separator (`\x1E`), respectively.
    pub fn ascii(&mut self) -> &mut ReaderBuilder {
        self.builder.ascii();
        self
    }

    /// Set the capacity (in bytes) of the buffer used in the CSV reader.
    ///
    /// Note that if a custom buffer is given with the `buffer` method, then
    /// this setting has no effect.
    pub fn buffer_capacity(&mut self, capacity: usize) -> &mut ReaderBuilder {
        self.capacity = capacity;
        self
    }

    /// Enable or disable the NFA for parsing CSV.
    ///
    /// This is intended to be a debug option useful for debugging. The NFA
    /// is always slower than the DFA.
    #[doc(hidden)]
    pub fn nfa(&mut self, yes: bool) -> &mut ReaderBuilder {
        self.builder.nfa(yes);
        self
    }
}

#[derive(Debug)]
pub struct Reader<R> {
    core: CoreReader,
    rdr: io::BufReader<R>,
    state: ReaderState,
}

#[derive(Debug)]
struct ReaderState {
    /// When set, this contains the first row of any parsed CSV data.
    ///
    /// This is always populated, regardless of whether `has_headers` is set.
    headers: Option<Headers>,
    /// When set, the first row of parsed CSV data is excluded from things
    /// that read records, like iterators and `read_record`.
    has_headers: bool,
    /// When set, there is no restriction on the length of records. When not
    /// set, every record must have the same number of fields, or else an error
    /// is reported.
    flexible: bool,
    /// The number of fields in the first record parsed.
    first_field_count: Option<u64>,
    /// The position of the parser just before the previous record was parsed.
    prev_pos: Position,
    /// The current position of the parser.
    ///
    /// Note that this position is only observable by callers at the start
    /// of a record. More granular positions are not supported.
    cur_pos: Position,
    /// Whether this reader has been seeked or not.
    seeked: bool,
    /// Whether the first record has been read or not.
    first: bool,
    /// Whether EOF of the underlying reader has been reached or not.
    eof: bool,
}

/// Headers encapsulates any data associated with the headers of CSV data.
///
/// The headers always correspond to the first row.
#[derive(Debug)]
struct Headers {
    /// The position just of the parser just before the headers were parsed,
    /// if available. This is unavailable when the caller sets the headers
    /// explicitly.
    pos: Option<Position>,
    /// The header, as raw bytes.
    byte_record: ByteRecord,
    /// The header, as valid UTF-8 (or a UTF-8 error).
    string_record: result::Result<StringRecord, Utf8Error>,
}

impl<R: io::Read> Reader<R> {
    /// Create a new CSV reader given a builder and a source of underlying
    /// bytes.
    fn new(builder: &ReaderBuilder, rdr: R) -> Reader<R> {
        Reader {
            core: builder.builder.build(),
            rdr: io::BufReader::with_capacity(builder.capacity, rdr),
            state: ReaderState {
                headers: None,
                has_headers: builder.has_headers,
                flexible: builder.flexible,
                first_field_count: None,
                prev_pos: Position::new(),
                cur_pos: Position::new(),
                seeked: false,
                first: false,
                eof: false,
            },
        }
    }

    /// Create a new CSV parser with a default configuration for the given
    /// file path.
    ///
    /// To customize CSV parsing, use a `ReaderBuilder`.
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Reader<File>> {
        ReaderBuilder::new().from_path(path)
    }

    /// Create a new CSV parser with a default configuration for the given
    /// reader.
    ///
    /// To customize CSV parsing, use a `ReaderBuilder`.
    pub fn from_reader(rdr: R) -> Reader<R> {
        ReaderBuilder::new().from_reader(rdr)
    }

    /// Returns a reference to the first row read by this parser.
    ///
    /// If no row has been read yet, then this will force parsing of the first
    /// row.
    ///
    /// If there was a problem parsing the row or if it wasn't valid UTF-8,
    /// then this returns an error.
    ///
    /// Note that this method may be used regardless of whether `has_headers`
    /// is enabled.
    pub fn headers(&mut self) -> Result<&StringRecord> {
        if self.state.headers.is_none() {
            if self.state.seeked {
                return Err(Error::Seek);
            }
            let mut record = ByteRecord::new();
            let pos = self.position().clone();
            self.read_record_bytes_impl(&mut record)?;
            self.set_headers_pos(Err(record), Some(pos));
        }
        let headers = self.state.headers.as_ref().unwrap();
        match headers.string_record {
            Ok(ref record) => Ok(record),
            Err(ref err) => Err(Error::Utf8 {
                pos: headers.pos.clone(),
                err: err.clone(),
            }),
        }
    }

    /// Set the headers of this CSV parser manually.
    ///
    /// This overrides any other setting. Any automatic detection of headers
    /// is disabled.
    pub fn set_headers(&mut self, headers: StringRecord) {
        self.set_headers_pos(Ok(headers), None);
    }

    /// Returns a reference to the first row read by this parser as raw bytes.
    ///
    /// If no row has been read yet, then this will force parsing of the first
    /// row.
    ///
    /// If there was a problem parsing the row then this returns an error.
    ///
    /// Note that this method may be used regardless of whether `has_headers`
    /// is enabled.
    pub fn byte_headers(&mut self) -> Result<&ByteRecord> {
        if self.state.headers.is_none() {
            if self.state.seeked {
                return Err(Error::Seek);
            }
            let mut record = ByteRecord::new();
            let pos = self.position().clone();
            self.read_record_bytes_impl(&mut record)?;
            self.set_headers_pos(Err(record), Some(pos));
        }
        Ok(&self.state.headers.as_ref().unwrap().byte_record)
    }

    /// Set the headers of this CSV parser manually as raw bytes.
    ///
    /// This overrides any other setting. Any automatic detection of headers
    /// is disabled.
    pub fn set_byte_headers(&mut self, headers: ByteRecord) {
        self.set_headers_pos(Err(headers), None);
    }

    fn set_headers_pos(
        &mut self,
        headers: result::Result<StringRecord, ByteRecord>,
        pos: Option<Position>,
    ) {
        // If we have string headers, then get byte headers. But if we have
        // byte headers, then get the string headers (or a UTF-8 error).
        let (str_headers, byte_headers) = match headers {
            Ok(string) => {
                let bytes = string.clone().into_byte_record();
                (Ok(string), bytes)
            }
            Err(bytes) => {
                match StringRecord::from_byte_record(bytes.clone()) {
                    Ok(str_headers) => (Ok(str_headers), bytes),
                    Err(err) => (Err(err.utf8_error().clone()), bytes),
                }
            }
        };
        self.state.headers = Some(Headers {
            pos: pos,
            byte_record: byte_headers,
            string_record: str_headers,
        });
    }

    /// Return the current position of this CSV reader.
    ///
    /// The byte offset in the position returned can be used to `seek` this
    /// reader. In particular, seeking to a position returned here on the same
    /// data will result in parsing the same subsequent record.
    pub fn position(&self) -> &Position {
        &self.state.cur_pos
    }

    pub fn read_record(&mut self, record: &mut StringRecord) -> Result<bool> {
        string_record::read(self, record)
    }

    pub fn read_record_bytes(
        &mut self,
        record: &mut ByteRecord,
    ) -> Result<bool> {
        if !self.state.has_headers && !self.state.first {
            if let Some(ref headers) = self.state.headers {
                self.state.first = true;
                record.clone_from(&headers.byte_record);
                return Ok(self.state.eof);
            }
        }
        let pos = self.position().clone();
        let eof = self.read_record_bytes_impl(record)?;
        self.state.first = true;
        if !self.state.seeked && self.state.headers.is_none() {
            self.set_headers_pos(Err(record.clone()), Some(pos));
            // If the end user indicated that we have headers, then we should
            // never return the first row. Instead, we should attempt to
            // read and return the next one.
            if self.state.has_headers {
                return self.read_record_bytes_impl(record);
            }
        }
        Ok(eof)
    }

    #[inline(always)]
    fn read_record_bytes_impl(
        &mut self,
        record: &mut ByteRecord,
    ) -> Result<bool> {
        use csv_core::ReadRecordResult::*;

        record.clear();
        if self.state.eof {
            return Ok(true);
        }
        let (mut outlen, mut endlen) = (0, 0);
        loop {
            let (res, nin, nout, nend) = {
                let input = self.rdr.fill_buf()?;
                let (mut fields, mut ends) = byte_record::as_parts(record);
                self.core.read_record(
                    input, &mut fields[outlen..], &mut ends[endlen..])
            };
            self.rdr.consume(nin);
            self.state.cur_pos.byte += nin as u64;
            self.state.cur_pos.line = self.core.line();
            outlen += nout;
            endlen += nend;
            match res {
                InputEmpty => continue,
                OutputFull => {
                    byte_record::expand_fields(record);
                    continue;
                }
                OutputEndsFull => {
                    byte_record::expand_ends(record);
                    continue;
                }
                Record => {
                    byte_record::set_len(record, endlen);
                    self.state.add_record(endlen as u64)?;
                    break;
                }
                End => {
                    self.state.eof = true;
                    break;
                }
            }
        }
        Ok(self.state.eof)
    }
}

impl<R: io::Read + io::Seek> Reader<R> {
    /// Seeks the underlying reader to the position given.
    ///
    /// This comes with a few caveats:
    ///
    /// * If the headers of this data have not already been read, then
    ///   `byte_headers` and `headers` will always return an error after a
    ///   call to `seek`.
    /// * Any internal buffer associated with this reader is cleared.
    /// * If the given position does not correspond to a position immediately
    ///   before the start of a record, then the behavior of this reader is
    ///   unspecified.
    ///
    /// If the given position has a byte offset equivalent to the current
    /// position, then no seeking is performed.
    pub fn seek(&mut self, pos: &Position) -> Result<()> {
        if pos.byte() == self.state.cur_pos.byte() {
            return Ok(());
        }
        self.seek_raw(io::SeekFrom::Start(pos.byte()), pos)
    }

    /// This is like `seek`, but provides direct control over how the seeking
    /// operation is performed via `io::SeekFrom`.
    ///
    /// The `pos` position given *should* correspond the position indicated
    /// by `seek_from`, but there is no requirement. If the `pos` position
    /// given is incorrect, then the position information returned by this
    /// reader will be similarly incorrect.
    ///
    /// Unlike `seek`, this will always cause an actual seek to be performed.
    pub fn seek_raw(
        &mut self,
        seek_from: io::SeekFrom,
        pos: &Position,
    ) -> Result<()> {
        self.rdr.seek(seek_from)?;
        self.core.reset();
        self.core.set_line(pos.line());
        self.state.seeked = true;
        self.state.prev_pos = pos.clone();
        self.state.cur_pos = pos.clone();
        self.state.eof = false;
        Ok(())
    }
}

impl ReaderState {
    #[inline(always)]
    fn add_record(&mut self, num_fields: u64) -> Result<()> {
        self.cur_pos.record = self.cur_pos.record.checked_add(1).unwrap();
        if !self.flexible {
            match self.first_field_count {
                None => self.first_field_count = Some(num_fields),
                Some(expected) => {
                    if num_fields != expected {
                        return Err(Error::UnequalLengths {
                            expected_len: expected,
                            pos: self.prev_pos.clone(),
                            len: num_fields,
                        });
                    }
                }
            }
        }
        self.prev_pos = self.cur_pos.clone();
        Ok(())
    }
}

/// A position in CSV data.
///
/// A position is used to report errors in CSV data. All positions include the
/// byte offset, line number and record index at which the error occurred.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Position {
    byte: u64,
    line: u64,
    record: u64,
}

impl Position {
    /// Returns a new position initialized to the start value.
    fn new() -> Position { Position { byte: 0, line: 1, record: 0 } }
    /// The byte offset, starting at `0`, of this position.
    pub fn byte(&self) -> u64 { self.byte }
    /// The line number, starting at `1`, of this position.
    pub fn line(&self) -> u64 { self.line }
    /// The record index, starting at `0`, of this position.
    pub fn record(&self) -> u64 { self.record }
}

#[cfg(test)]
mod tests {
    use std::io;

    use byte_record::ByteRecord;
    use error::{Error, new_utf8_error};
    use string_record::StringRecord;

    use super::{ReaderBuilder, Position};

    fn b(s: &str) -> &[u8] { s.as_bytes() }
    fn s(b: &[u8]) -> &str { ::std::str::from_utf8(b).unwrap() }

    macro_rules! assert_match {
        ($e:expr, $p:pat) => {{
            match $e {
                $p => {}
                e => panic!("match failed, got {:?}", e),
            }
        }}
    }

    #[test]
    fn read_record_bytes() {
        let data = b("foo,\"b,ar\",baz\nabc,mno,xyz");
        let mut rdr = ReaderBuilder::new()
            .has_headers(false)
            .from_reader(data);
        let mut rec = ByteRecord::new();

        assert!(!rdr.read_record_bytes(&mut rec).unwrap());
        assert_eq!(3, rec.len());
        assert_eq!("foo", s(&rec[0]));
        assert_eq!("b,ar", s(&rec[1]));
        assert_eq!("baz", s(&rec[2]));

        assert!(!rdr.read_record_bytes(&mut rec).unwrap());
        assert_eq!(3, rec.len());
        assert_eq!("abc", s(&rec[0]));
        assert_eq!("mno", s(&rec[1]));
        assert_eq!("xyz", s(&rec[2]));

        assert!(rdr.read_record_bytes(&mut rec).unwrap());
    }

    #[test]
    fn read_record_unequal_fails() {
        let data = b("foo\nbar,baz");
        let mut rdr = ReaderBuilder::new()
            .has_headers(false)
            .from_reader(data);
        let mut rec = ByteRecord::new();

        assert!(!rdr.read_record_bytes(&mut rec).unwrap());
        assert_eq!(1, rec.len());
        assert_eq!("foo", s(&rec[0]));

        assert_match!(
            rdr.read_record_bytes(&mut rec),
            Err(Error::UnequalLengths {
                expected_len: 1,
                pos: Position { byte: 4, line: 2, record: 1},
                len: 2,
            }));
    }

    #[test]
    fn read_record_unequal_ok() {
        let data = b("foo\nbar,baz");
        let mut rdr = ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .from_reader(data);
        let mut rec = ByteRecord::new();

        assert!(!rdr.read_record_bytes(&mut rec).unwrap());
        assert_eq!(1, rec.len());
        assert_eq!("foo", s(&rec[0]));

        assert!(!rdr.read_record_bytes(&mut rec).unwrap());
        assert_eq!(2, rec.len());
        assert_eq!("bar", s(&rec[0]));
        assert_eq!("baz", s(&rec[1]));

        assert!(rdr.read_record_bytes(&mut rec).unwrap());
    }

    // This tests that even if we get a CSV error, we can continue reading
    // if we want.
    #[test]
    fn read_record_unequal_continue() {
        let data = b("foo\nbar,baz\nquux");
        let mut rdr = ReaderBuilder::new()
            .has_headers(false)
            .from_reader(data);
        let mut rec = ByteRecord::new();

        assert!(!rdr.read_record_bytes(&mut rec).unwrap());
        assert_eq!(1, rec.len());
        assert_eq!("foo", s(&rec[0]));

        assert_match!(
            rdr.read_record_bytes(&mut rec),
            Err(Error::UnequalLengths {
                expected_len: 1,
                pos: Position { byte: 4, line: 2, record: 1},
                len: 2,
            }));

        assert!(!rdr.read_record_bytes(&mut rec).unwrap());
        assert_eq!(1, rec.len());
        assert_eq!("quux", s(&rec[0]));

        assert!(rdr.read_record_bytes(&mut rec).unwrap());
    }

    #[test]
    fn read_record_headers() {
        let data = b("foo,bar,baz\na,b,c\nd,e,f");
        let mut rdr = ReaderBuilder::new().has_headers(true).from_reader(data);
        let mut rec = StringRecord::new();

        assert!(!rdr.read_record(&mut rec).unwrap());
        assert_eq!(3, rec.len());
        assert_eq!("a", &rec[0]);

        assert!(!rdr.read_record(&mut rec).unwrap());
        assert_eq!(3, rec.len());
        assert_eq!("d", &rec[0]);

        assert!(rdr.read_record(&mut rec).unwrap());

        {
            let headers = rdr.byte_headers().unwrap();
            assert_eq!(3, headers.len());
            assert_eq!(b"foo", &headers[0]);
            assert_eq!(b"bar", &headers[1]);
            assert_eq!(b"baz", &headers[2]);
        }
        {
            let headers = rdr.headers().unwrap();
            assert_eq!(3, headers.len());
            assert_eq!("foo", &headers[0]);
            assert_eq!("bar", &headers[1]);
            assert_eq!("baz", &headers[2]);
        }
    }

    #[test]
    fn read_record_headers_invalid_utf8() {
        let data = &b"foo,b\xFFar,baz\na,b,c\nd,e,f"[..];
        let mut rdr = ReaderBuilder::new().has_headers(true).from_reader(data);
        let mut rec = StringRecord::new();

        assert!(!rdr.read_record(&mut rec).unwrap());
        assert_eq!(3, rec.len());
        assert_eq!("a", &rec[0]);

        assert!(!rdr.read_record(&mut rec).unwrap());
        assert_eq!(3, rec.len());
        assert_eq!("d", &rec[0]);

        assert!(rdr.read_record(&mut rec).unwrap());

        // Check that we can read the headers as raw bytes, but that
        // if we read them as strings, we get an appropriate UTF-8 error.
        {
            let headers = rdr.byte_headers().unwrap();
            assert_eq!(3, headers.len());
            assert_eq!(b"foo", &headers[0]);
            assert_eq!(b"b\xFFar", &headers[1]);
            assert_eq!(b"baz", &headers[2]);
        }
        match rdr.headers().unwrap_err() {
            Error::Utf8 { pos: Some(pos), err } => {
                assert_eq!(pos, Position { byte: 0, line: 1, record: 0 });
                assert_eq!(err.field(), 1);
                assert_eq!(err.valid_up_to(), 1);
            }
            err => panic!("match failed, got {:?}", err),
        }
    }

    #[test]
    fn read_record_no_headers_before() {
        let data = b("foo,bar,baz\na,b,c\nd,e,f");
        let mut rdr = ReaderBuilder::new()
            .has_headers(false)
            .from_reader(data);
        let mut rec = StringRecord::new();

        {
            let headers = rdr.headers().unwrap();
            assert_eq!(3, headers.len());
            assert_eq!("foo", &headers[0]);
            assert_eq!("bar", &headers[1]);
            assert_eq!("baz", &headers[2]);
        }

        assert!(!rdr.read_record(&mut rec).unwrap());
        assert_eq!(3, rec.len());
        assert_eq!("foo", &rec[0]);

        assert!(!rdr.read_record(&mut rec).unwrap());
        assert_eq!(3, rec.len());
        assert_eq!("a", &rec[0]);

        assert!(!rdr.read_record(&mut rec).unwrap());
        assert_eq!(3, rec.len());
        assert_eq!("d", &rec[0]);

        assert!(rdr.read_record(&mut rec).unwrap());
    }

    #[test]
    fn read_record_no_headers_after() {
        let data = b("foo,bar,baz\na,b,c\nd,e,f");
        let mut rdr = ReaderBuilder::new()
            .has_headers(false)
            .from_reader(data);
        let mut rec = StringRecord::new();

        assert!(!rdr.read_record(&mut rec).unwrap());
        assert_eq!(3, rec.len());
        assert_eq!("foo", &rec[0]);

        assert!(!rdr.read_record(&mut rec).unwrap());
        assert_eq!(3, rec.len());
        assert_eq!("a", &rec[0]);

        assert!(!rdr.read_record(&mut rec).unwrap());
        assert_eq!(3, rec.len());
        assert_eq!("d", &rec[0]);

        assert!(rdr.read_record(&mut rec).unwrap());

        let headers = rdr.headers().unwrap();
        assert_eq!(3, headers.len());
        assert_eq!("foo", &headers[0]);
        assert_eq!("bar", &headers[1]);
        assert_eq!("baz", &headers[2]);
    }

    #[test]
    fn seek() {
        let data = b("foo,bar,baz\na,b,c\nd,e,f\ng,h,i");
        let mut rdr = ReaderBuilder::new()
            .from_reader(io::Cursor::new(data));
        let pos = Position { byte: 18, line: 3, record: 2 };
        rdr.seek(&pos).unwrap();

        let mut rec = StringRecord::new();

        assert_eq!(18, rdr.position().byte());
        assert!(!rdr.read_record(&mut rec).unwrap());
        assert_eq!(3, rec.len());
        assert_eq!("d", &rec[0]);

        assert_eq!(24, rdr.position().byte());
        assert_eq!(4, rdr.position().line());
        assert_eq!(3, rdr.position().record());
        assert!(!rdr.read_record(&mut rec).unwrap());
        assert_eq!(3, rec.len());
        assert_eq!("g", &rec[0]);

        assert!(rdr.read_record(&mut rec).unwrap());
    }

    // Test that asking for headers after a seek returns an error if the
    // headers weren't read before seeking.
    #[test]
    fn seek_headers_error() {
        let data = b("foo,bar,baz\na,b,c\nd,e,f\ng,h,i");
        let mut rdr = ReaderBuilder::new()
            .from_reader(io::Cursor::new(data));
        let pos = Position { byte: 18, line: 3, record: 2 };
        rdr.seek(&pos).unwrap();
        assert_match!(rdr.headers(), Err(Error::Seek));
    }

    // Test that we can read headers after seeking if the headers were read
    // before seeking.
    #[test]
    fn seek_headers() {
        let data = b("foo,bar,baz\na,b,c\nd,e,f\ng,h,i");
        let mut rdr = ReaderBuilder::new()
            .from_reader(io::Cursor::new(data));
        let headers = rdr.headers().unwrap().clone();
        let pos = Position { byte: 18, line: 3, record: 2 };
        rdr.seek(&pos).unwrap();
        assert_eq!(&headers, rdr.headers().unwrap());
    }

    // Test that even if we didn't read headers before seeking, if we seek to
    // the current byte offset, then no seeking is done and therefore we can
    // still read headers after seeking.
    #[test]
    fn seek_headers_no_actual_seek() {
        let data = b("foo,bar,baz\na,b,c\nd,e,f\ng,h,i");
        let mut rdr = ReaderBuilder::new()
            .from_reader(io::Cursor::new(data));
        rdr.seek(&Position::new()).unwrap();
        assert_eq!("foo", &rdr.headers().unwrap()[0]);
    }
}
