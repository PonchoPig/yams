use std::ffi::OsStr;
use std::os::fd::{AsFd, BorrowedFd};
#[cfg(test)]
use std::path::PathBuf;
use std::path::{Component, Path};

use rustix::fs::{self as rfs, AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use rustix::process::geteuid;
use sha2::{Digest, Sha256};

/// Maximum UTF-8 byte length accepted for a query before hashing.
pub const MAX_QUERY_BYTES: usize = 1 << 20;
/// Maximum UTF-8 byte length accepted for a canonical project path.
pub const MAX_PROJECT_BYTES: usize = 16 << 10;
const MAX_TIMESTAMP_BYTES: usize = 64;
const MAX_RECORD_BYTES: usize = 64 << 10;

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::DIRECTORY);
const LOG_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::APPEND)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);

/// Why the composing operation is or is not eligible for query logging.
///
/// The caller must classify every return path. Only a search that reached the
/// retrieval attempt can append a record; management, mutation, validation,
/// and operational failures are excluded from the diagnostic denominator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryLogEligibility {
    SearchAttempted,
    Management,
    Write,
    BlankOrInvalidQuery,
    PreSearchFailure,
    OperationalFailure,
}

/// Presentation-independent values for one completed search attempt.
///
/// `timestamp` is injected by the caller so this utility has no global clock.
/// `project` must already be canonical. The query itself is only used to
/// calculate `q` and is never serialized.
pub struct QueryLogRecord<'a> {
    pub timestamp: &'a str,
    pub project: &'a Path,
    pub query: &'a str,
    pub k: u32,
    pub rc: i32,
    pub hits: u64,
    pub gate: bool,
    pub explain: bool,
    pub min_score: Option<f64>,
    pub max_gap: Option<f64>,
    pub all: bool,
}

/// A reason a record was deliberately excluded before touching the log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryLogSkip {
    Ineligible(QueryLogEligibility),
    InvalidRecord,
    Oversized,
}

/// Internal observability for tests and metrics; never a caller-facing error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub enum QueryLogOutcome {
    Appended,
    Disabled,
    Skipped(QueryLogSkip),
    Rejected,
    Failed,
}

/// Append one query record when the explicitly selected log already exists.
///
/// Every filesystem and write error is represented only by the returned
/// outcome. The function never creates the path and never writes diagnostics.
pub fn append_query_log(
    path: &Path,
    eligibility: QueryLogEligibility,
    record: &QueryLogRecord<'_>,
) -> QueryLogOutcome {
    let mut runtime = SystemRuntime;
    append_with_runtime(path, eligibility, record, geteuid().as_raw(), &mut runtime)
}

/// Stable query identity matching the project Python 3.12 / Unicode 15
/// `query.strip().lower()` behavior.
pub fn query_hash(query: &str) -> String {
    let normalized = python_lowercase(python_trim(query));
    let digest = Sha256::digest(normalized.as_bytes());
    let mut short = String::with_capacity(16);
    for byte in &digest[..8] {
        push_hex_byte(&mut short, *byte);
    }
    short
}

fn append_with_runtime(
    path: &Path,
    eligibility: QueryLogEligibility,
    record: &QueryLogRecord<'_>,
    expected_uid: u32,
    runtime: &mut impl Runtime,
) -> QueryLogOutcome {
    if eligibility != QueryLogEligibility::SearchAttempted {
        return QueryLogOutcome::Skipped(QueryLogSkip::Ineligible(eligibility));
    }

    let line = match compose_line(record) {
        Ok(line) => line,
        Err(skip) => return QueryLogOutcome::Skipped(skip),
    };

    match append_existing(path, line.as_bytes(), expected_uid, runtime) {
        Ok(()) => QueryLogOutcome::Appended,
        Err(AppendError::Missing) => QueryLogOutcome::Disabled,
        Err(AppendError::Unsafe) => QueryLogOutcome::Rejected,
        Err(AppendError::Io) => QueryLogOutcome::Failed,
    }
}

fn compose_line(record: &QueryLogRecord<'_>) -> Result<String, QueryLogSkip> {
    if record.query.len() > MAX_QUERY_BYTES {
        return Err(QueryLogSkip::Oversized);
    }
    if python_trim(record.query).is_empty() || !matches!(record.rc, 0 | 1 | 3) {
        return Err(QueryLogSkip::InvalidRecord);
    }
    if record.timestamp.is_empty()
        || record.timestamp.len() > MAX_TIMESTAMP_BYTES
        || !is_utc_millisecond_timestamp(record.timestamp)
        || !record.project.is_absolute()
        || !is_lexically_canonical(record.project)
    {
        return Err(QueryLogSkip::InvalidRecord);
    }
    let project = record.project.to_str().ok_or(QueryLogSkip::InvalidRecord)?;
    if project.len() > MAX_PROJECT_BYTES {
        return Err(QueryLogSkip::Oversized);
    }
    if record.min_score.is_some_and(|value| !value.is_finite())
        || record.max_gap.is_some_and(|value| !value.is_finite())
    {
        return Err(QueryLogSkip::InvalidRecord);
    }

    let mut line = String::with_capacity(256 + project.len());
    line.push_str("{\"ts\":");
    push_json_string(&mut line, record.timestamp);
    line.push_str(",\"project\":");
    push_json_string(&mut line, project);
    line.push_str(",\"q\":\"");
    line.push_str(&query_hash(record.query));
    line.push_str("\",\"k\":");
    line.push_str(&record.k.to_string());
    line.push_str(",\"rc\":");
    line.push_str(&record.rc.to_string());
    line.push_str(",\"hits\":");
    line.push_str(&record.hits.to_string());
    line.push_str(",\"gate\":");
    push_bool(&mut line, record.gate);
    line.push_str(",\"explain\":");
    push_bool(&mut line, record.explain);
    line.push_str(",\"min_score\":");
    push_optional_number(&mut line, record.min_score);
    line.push_str(",\"max_gap\":");
    push_optional_number(&mut line, record.max_gap);
    line.push_str(",\"all\":");
    push_bool(&mut line, record.all);
    line.push_str("}\n");
    if line.len() > MAX_RECORD_BYTES {
        return Err(QueryLogSkip::Oversized);
    }
    Ok(line)
}

fn append_existing(
    path: &Path,
    bytes: &[u8],
    expected_uid: u32,
    runtime: &mut impl Runtime,
) -> Result<(), AppendError> {
    let parent = path.parent().ok_or(AppendError::Unsafe)?;
    let name = path.file_name().ok_or(AppendError::Unsafe)?;
    let directory = match rfs::open(parent, DIRECTORY_FLAGS, Mode::empty()) {
        Ok(directory) => directory,
        Err(Errno::NOENT) => return Err(AppendError::Missing),
        Err(_) => return Err(AppendError::Io),
    };

    let first_named = named_state(directory.as_fd(), name)?;
    validate_log_state(&first_named, expected_uid)?;
    let descriptor = match rfs::openat(&directory, name, LOG_FLAGS, Mode::empty()) {
        Ok(descriptor) => descriptor,
        Err(Errno::NOENT) => return Err(AppendError::Missing),
        Err(Errno::LOOP) => return Err(AppendError::Unsafe),
        Err(_) => return Err(AppendError::Io),
    };
    let first_opened = descriptor_state(descriptor.as_fd())?;
    validate_log_state(&first_opened, expected_uid)?;
    if Identity::from_stat(&first_named) != Identity::from_stat(&first_opened) {
        return Err(AppendError::Unsafe);
    }

    // Revalidate both bindings immediately before the single append syscall.
    runtime
        .before_final_validation()
        .map_err(|()| AppendError::Io)?;
    let final_named = named_state(directory.as_fd(), name)?;
    let final_opened = descriptor_state(descriptor.as_fd())?;
    validate_log_state(&final_named, expected_uid)?;
    validate_log_state(&final_opened, expected_uid)?;
    let identity = Identity::from_stat(&first_opened);
    if Identity::from_stat(&final_named) != identity
        || Identity::from_stat(&final_opened) != identity
    {
        return Err(AppendError::Unsafe);
    }

    match runtime.write(descriptor.as_fd(), bytes) {
        Ok(written) if written == bytes.len() => Ok(()),
        Ok(_) | Err(_) => Err(AppendError::Io),
    }
}

fn named_state(directory: BorrowedFd<'_>, name: &OsStr) -> Result<Stat, AppendError> {
    match rfs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(stat),
        Err(Errno::NOENT) => Err(AppendError::Missing),
        Err(_) => Err(AppendError::Io),
    }
}

fn descriptor_state(descriptor: BorrowedFd<'_>) -> Result<Stat, AppendError> {
    rfs::fstat(descriptor).map_err(|_| AppendError::Io)
}

fn validate_log_state(stat: &Stat, expected_uid: u32) -> Result<(), AppendError> {
    if !FileType::from_raw_mode(stat.st_mode).is_file()
        || owner_id(stat) != expected_uid
        || stat.st_nlink != 1
        || stat.st_mode as u32 & 0o077 != 0
    {
        return Err(AppendError::Unsafe);
    }
    Ok(())
}

#[allow(clippy::unnecessary_cast)]
fn owner_id(stat: &Stat) -> u32 {
    stat.st_uid as u32
}

fn is_lexically_canonical(path: &Path) -> bool {
    path.components()
        .all(|component| !matches!(component, Component::CurDir | Component::ParentDir))
}

fn is_utc_millisecond_timestamp(timestamp: &str) -> bool {
    let bytes = timestamp.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return false;
    }
    for index in [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18, 20, 21, 22] {
        if !bytes[index].is_ascii_digit() {
            return false;
        }
    }
    let year = decimal(bytes, 0, 4);
    let month = decimal(bytes, 5, 7);
    let day = decimal(bytes, 8, 10);
    let hour = decimal(bytes, 11, 13);
    let minute = decimal(bytes, 14, 16);
    let second = decimal(bytes, 17, 19);
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return false,
    };
    (1..=max_day).contains(&day) && hour < 24 && minute < 60 && second < 60
}

fn decimal(bytes: &[u8], start: usize, end: usize) -> u32 {
    bytes[start..end]
        .iter()
        .fold(0, |value, byte| value * 10 + u32::from(byte - b'0'))
}

fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn python_trim(input: &str) -> &str {
    input.trim_matches(is_python_whitespace)
}

fn is_python_whitespace(character: char) -> bool {
    character.is_whitespace() || matches!(character, '\u{001c}'..='\u{001f}')
}

fn python_lowercase(input: &str) -> String {
    let characters = input.chars().collect::<Vec<_>>();
    let mut lowered = String::with_capacity(input.len());
    for (index, character) in characters.iter().copied().enumerate() {
        if character == 'Σ' && is_final_sigma(&characters, index) {
            lowered.push('ς');
        } else {
            push_unicode_15_lowercase(&mut lowered, character);
        }
    }
    lowered
}

// Unicode's only language-neutral contextual lowercase mapping is Greek final
// sigma. Locale-specific SpecialCasing rules do not participate in Python's
// locale-independent str.lower().
fn is_final_sigma(characters: &[char], index: usize) -> bool {
    let cased_before = characters[..index]
        .iter()
        .rev()
        .copied()
        .find(|character| !is_case_ignorable(*character))
        .is_some_and(is_cased);
    let cased_after = characters[index + 1..]
        .iter()
        .copied()
        .find(|character| !is_case_ignorable(*character))
        .is_some_and(is_cased);
    cased_before && !cased_after
}

fn is_cased(character: char) -> bool {
    in_unicode_ranges(character, UNICODE_15_CASED)
}

fn is_case_ignorable(character: char) -> bool {
    in_unicode_ranges(character, UNICODE_15_CASE_IGNORABLE)
}

fn push_unicode_15_lowercase(output: &mut String, character: char) {
    // The only unconditional multi-scalar lowercase mapping in Unicode 15.
    if character == '\u{0130}' {
        output.push('i');
        output.push('\u{0307}');
        return;
    }

    let codepoint = character as u32;
    let index = UNICODE_15_LOWERCASE.partition_point(|(_, end, _, _)| *end < codepoint);
    let Some(&(start, end, step, delta)) = UNICODE_15_LOWERCASE.get(index) else {
        output.push(character);
        return;
    };
    if codepoint < start || codepoint > end || !(codepoint - start).is_multiple_of(step) {
        output.push(character);
        return;
    }

    let mapped = i64::from(codepoint) + i64::from(delta);
    let mapped = u32::try_from(mapped)
        .ok()
        .and_then(char::from_u32)
        .expect("generated Unicode 15 lowercase mapping remains a Unicode scalar");
    output.push(mapped);
}

fn in_unicode_ranges(character: char, ranges: &[(u32, u32)]) -> bool {
    let codepoint = character as u32;
    let index = ranges.partition_point(|(_, end)| *end < codepoint);
    ranges
        .get(index)
        .is_some_and(|(start, end)| *start <= codepoint && codepoint <= *end)
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{0008}' => output.push_str("\\b"),
            '\t' => output.push_str("\\t"),
            '\n' => output.push_str("\\n"),
            '\u{000c}' => output.push_str("\\f"),
            '\r' => output.push_str("\\r"),
            '\u{0000}'..='\u{001f}' => {
                output.push_str("\\u00");
                push_hex_byte(output, character as u8);
            }
            _ => output.push(character),
        }
    }
    output.push('"');
}

fn push_hex_byte(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    output.push(HEX[usize::from(byte >> 4)] as char);
    output.push(HEX[usize::from(byte & 0x0f)] as char);
}

fn push_bool(output: &mut String, value: bool) {
    output.push_str(if value { "true" } else { "false" });
}

fn push_optional_number(output: &mut String, value: Option<f64>) {
    if let Some(value) = value {
        output.push_str(&value.to_string());
    } else {
        output.push_str("null");
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Identity {
    device: u128,
    inode: u128,
}

impl Identity {
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(stat: &Stat) -> Self {
        Self {
            device: stat.st_dev as u128,
            inode: stat.st_ino as u128,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppendError {
    Missing,
    Unsafe,
    Io,
}

trait Runtime {
    fn before_final_validation(&mut self) -> Result<(), ()> {
        Ok(())
    }

    fn write(&mut self, descriptor: BorrowedFd<'_>, bytes: &[u8]) -> Result<usize, Errno>;
}

struct SystemRuntime;

impl Runtime for SystemRuntime {
    fn write(&mut self, descriptor: BorrowedFd<'_>, bytes: &[u8]) -> Result<usize, Errno> {
        rustix::io::write(descriptor, bytes)
    }
}

#[cfg(test)]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TestWriteBehavior {
    Normal,
    Fail,
    Short,
    RebindBeforeFinalValidation,
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn append_query_log_with_test_policy(
    path: &Path,
    eligibility: QueryLogEligibility,
    record: &QueryLogRecord<'_>,
    expected_uid: u32,
    behavior: TestWriteBehavior,
) -> QueryLogOutcome {
    let mut runtime = TestRuntime {
        behavior,
        path: path.to_path_buf(),
        writes: 0,
    };
    append_with_runtime(path, eligibility, record, expected_uid, &mut runtime)
}

#[cfg(test)]
#[allow(dead_code)]
struct TestRuntime {
    behavior: TestWriteBehavior,
    path: PathBuf,
    writes: usize,
}

#[cfg(test)]
impl Runtime for TestRuntime {
    fn before_final_validation(&mut self) -> Result<(), ()> {
        if self.behavior != TestWriteBehavior::RebindBeforeFinalValidation {
            return Ok(());
        }
        let mut displaced = self.path.as_os_str().to_owned();
        displaced.push(".displaced");
        std::fs::rename(&self.path, PathBuf::from(displaced)).map_err(|_| ())?;
        let replacement = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)
            .map_err(|_| ())?;
        rfs::fchmod(&replacement, Mode::RUSR | Mode::WUSR).map_err(|_| ())?;
        Ok(())
    }

    fn write(&mut self, descriptor: BorrowedFd<'_>, bytes: &[u8]) -> Result<usize, Errno> {
        self.writes += 1;
        assert_eq!(self.writes, 1, "query logging must use exactly one write");
        match self.behavior {
            TestWriteBehavior::Normal => rustix::io::write(descriptor, bytes),
            TestWriteBehavior::Fail => Err(Errno::IO),
            TestWriteBehavior::Short => {
                rustix::io::write(descriptor, &bytes[..bytes.len().saturating_sub(1)])
            }
            TestWriteBehavior::RebindBeforeFinalValidation => {
                panic!("a rebound name must be rejected before writing")
            }
        }
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn unicode_15_lowercase_for_test(input: &str) -> String {
    python_lowercase(input)
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn unicode_15_is_cased_for_test(character: char) -> bool {
    is_cased(character)
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn unicode_15_is_case_ignorable_for_test(character: char) -> bool {
    is_case_ignorable(character)
}

// Pinned Unicode Character Database 15.0.0 sources:
// https://www.unicode.org/Public/15.0.0/ucd/DerivedCoreProperties.txt
// DerivedCoreProperties.txt SHA-256
// d367290bc0867e6b484c68370530bdd1a08b6b32404601b8c7accaf83e05628d
// https://www.unicode.org/Public/15.0.0/ucd/SpecialCasing.txt
// SpecialCasing.txt SHA-256
// 78b29c64b5840d25c11a9f31b665ee551b8a499eca6c70d770fcad7dd710f494
// https://www.unicode.org/Public/15.0.0/ucd/UnicodeData.txt
// UnicodeData.txt SHA-256
// 806e9aed65037197f1ec85e12be6e8cd870fc5608b4de0fffd990f689f376a73
//
// UNICODE LICENSE V3
//
// COPYRIGHT AND PERMISSION NOTICE
//
// Copyright © 1991-2026 Unicode, Inc.
//
// NOTICE TO USER: Carefully read the following legal agreement. BY
// DOWNLOADING, INSTALLING, COPYING OR OTHERWISE USING DATA FILES, AND/OR
// SOFTWARE, YOU UNEQUIVOCALLY ACCEPT, AND AGREE TO BE BOUND BY, ALL OF THE
// TERMS AND CONDITIONS OF THIS AGREEMENT. IF YOU DO NOT AGREE, DO NOT
// DOWNLOAD, INSTALL, COPY, DISTRIBUTE OR USE THE DATA FILES OR SOFTWARE.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of data files and any associated documentation (the "Data Files") or
// software and any associated documentation (the "Software") to deal in the
// Data Files or Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, and/or sell
// copies of the Data Files or Software, and to permit persons to whom the Data
// Files or Software are furnished to do so, provided that either (a) this
// copyright and permission notice appear with all copies of the Data Files or
// Software, or (b) this copyright and permission notice appear in associated
// Documentation.
//
// THE DATA FILES AND SOFTWARE ARE PROVIDED "AS IS", WITHOUT WARRANTY OF ANY
// KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT OF
// THIRD PARTY RIGHTS.
//
// IN NO EVENT SHALL THE COPYRIGHT HOLDER OR HOLDERS INCLUDED IN THIS NOTICE
// BE LIABLE FOR ANY CLAIM, OR ANY SPECIAL INDIRECT OR CONSEQUENTIAL DAMAGES,
// OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS,
// WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION,
// ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THE DATA
// FILES OR SOFTWARE.
//
// Except as contained in this notice, the name of a copyright holder shall
// not be used in advertising or otherwise to promote the sale, use or other
// dealings in these Data Files or Software without prior written
// authorization of the copyright holder.
// Generated from Unicode 15.0.0 UnicodeData.txt and unconditional SpecialCasing.txt.
const UNICODE_15_LOWERCASE: &[(u32, u32, u32, i32)] = &[
    (0x0041, 0x005A, 1, 32),
    (0x00C0, 0x00D6, 1, 32),
    (0x00D8, 0x00DE, 1, 32),
    (0x0100, 0x012E, 2, 1),
    (0x0132, 0x0136, 2, 1),
    (0x0139, 0x0147, 2, 1),
    (0x014A, 0x0176, 2, 1),
    (0x0178, 0x0178, 1, -121),
    (0x0179, 0x017D, 2, 1),
    (0x0181, 0x0181, 1, 210),
    (0x0182, 0x0184, 2, 1),
    (0x0186, 0x0186, 1, 206),
    (0x0187, 0x0187, 1, 1),
    (0x0189, 0x018A, 1, 205),
    (0x018B, 0x018B, 1, 1),
    (0x018E, 0x018E, 1, 79),
    (0x018F, 0x018F, 1, 202),
    (0x0190, 0x0190, 1, 203),
    (0x0191, 0x0191, 1, 1),
    (0x0193, 0x0193, 1, 205),
    (0x0194, 0x0194, 1, 207),
    (0x0196, 0x0196, 1, 211),
    (0x0197, 0x0197, 1, 209),
    (0x0198, 0x0198, 1, 1),
    (0x019C, 0x019C, 1, 211),
    (0x019D, 0x019D, 1, 213),
    (0x019F, 0x019F, 1, 214),
    (0x01A0, 0x01A4, 2, 1),
    (0x01A6, 0x01A6, 1, 218),
    (0x01A7, 0x01A7, 1, 1),
    (0x01A9, 0x01A9, 1, 218),
    (0x01AC, 0x01AC, 1, 1),
    (0x01AE, 0x01AE, 1, 218),
    (0x01AF, 0x01AF, 1, 1),
    (0x01B1, 0x01B2, 1, 217),
    (0x01B3, 0x01B5, 2, 1),
    (0x01B7, 0x01B7, 1, 219),
    (0x01B8, 0x01BC, 4, 1),
    (0x01C4, 0x01C4, 1, 2),
    (0x01C5, 0x01C5, 1, 1),
    (0x01C7, 0x01C7, 1, 2),
    (0x01C8, 0x01C8, 1, 1),
    (0x01CA, 0x01CA, 1, 2),
    (0x01CB, 0x01DB, 2, 1),
    (0x01DE, 0x01EE, 2, 1),
    (0x01F1, 0x01F1, 1, 2),
    (0x01F2, 0x01F4, 2, 1),
    (0x01F6, 0x01F6, 1, -97),
    (0x01F7, 0x01F7, 1, -56),
    (0x01F8, 0x021E, 2, 1),
    (0x0220, 0x0220, 1, -130),
    (0x0222, 0x0232, 2, 1),
    (0x023A, 0x023A, 1, 10795),
    (0x023B, 0x023B, 1, 1),
    (0x023D, 0x023D, 1, -163),
    (0x023E, 0x023E, 1, 10792),
    (0x0241, 0x0241, 1, 1),
    (0x0243, 0x0243, 1, -195),
    (0x0244, 0x0244, 1, 69),
    (0x0245, 0x0245, 1, 71),
    (0x0246, 0x024E, 2, 1),
    (0x0370, 0x0372, 2, 1),
    (0x0376, 0x0376, 1, 1),
    (0x037F, 0x037F, 1, 116),
    (0x0386, 0x0386, 1, 38),
    (0x0388, 0x038A, 1, 37),
    (0x038C, 0x038C, 1, 64),
    (0x038E, 0x038F, 1, 63),
    (0x0391, 0x03A1, 1, 32),
    (0x03A3, 0x03AB, 1, 32),
    (0x03CF, 0x03CF, 1, 8),
    (0x03D8, 0x03EE, 2, 1),
    (0x03F4, 0x03F4, 1, -60),
    (0x03F7, 0x03F7, 1, 1),
    (0x03F9, 0x03F9, 1, -7),
    (0x03FA, 0x03FA, 1, 1),
    (0x03FD, 0x03FF, 1, -130),
    (0x0400, 0x040F, 1, 80),
    (0x0410, 0x042F, 1, 32),
    (0x0460, 0x0480, 2, 1),
    (0x048A, 0x04BE, 2, 1),
    (0x04C0, 0x04C0, 1, 15),
    (0x04C1, 0x04CD, 2, 1),
    (0x04D0, 0x052E, 2, 1),
    (0x0531, 0x0556, 1, 48),
    (0x10A0, 0x10C5, 1, 7264),
    (0x10C7, 0x10CD, 6, 7264),
    (0x13A0, 0x13EF, 1, 38864),
    (0x13F0, 0x13F5, 1, 8),
    (0x1C90, 0x1CBA, 1, -3008),
    (0x1CBD, 0x1CBF, 1, -3008),
    (0x1E00, 0x1E94, 2, 1),
    (0x1E9E, 0x1E9E, 1, -7615),
    (0x1EA0, 0x1EFE, 2, 1),
    (0x1F08, 0x1F0F, 1, -8),
    (0x1F18, 0x1F1D, 1, -8),
    (0x1F28, 0x1F2F, 1, -8),
    (0x1F38, 0x1F3F, 1, -8),
    (0x1F48, 0x1F4D, 1, -8),
    (0x1F59, 0x1F5F, 2, -8),
    (0x1F68, 0x1F6F, 1, -8),
    (0x1F88, 0x1F8F, 1, -8),
    (0x1F98, 0x1F9F, 1, -8),
    (0x1FA8, 0x1FAF, 1, -8),
    (0x1FB8, 0x1FB9, 1, -8),
    (0x1FBA, 0x1FBB, 1, -74),
    (0x1FBC, 0x1FBC, 1, -9),
    (0x1FC8, 0x1FCB, 1, -86),
    (0x1FCC, 0x1FCC, 1, -9),
    (0x1FD8, 0x1FD9, 1, -8),
    (0x1FDA, 0x1FDB, 1, -100),
    (0x1FE8, 0x1FE9, 1, -8),
    (0x1FEA, 0x1FEB, 1, -112),
    (0x1FEC, 0x1FEC, 1, -7),
    (0x1FF8, 0x1FF9, 1, -128),
    (0x1FFA, 0x1FFB, 1, -126),
    (0x1FFC, 0x1FFC, 1, -9),
    (0x2126, 0x2126, 1, -7517),
    (0x212A, 0x212A, 1, -8383),
    (0x212B, 0x212B, 1, -8262),
    (0x2132, 0x2132, 1, 28),
    (0x2160, 0x216F, 1, 16),
    (0x2183, 0x2183, 1, 1),
    (0x24B6, 0x24CF, 1, 26),
    (0x2C00, 0x2C2F, 1, 48),
    (0x2C60, 0x2C60, 1, 1),
    (0x2C62, 0x2C62, 1, -10743),
    (0x2C63, 0x2C63, 1, -3814),
    (0x2C64, 0x2C64, 1, -10727),
    (0x2C67, 0x2C6B, 2, 1),
    (0x2C6D, 0x2C6D, 1, -10780),
    (0x2C6E, 0x2C6E, 1, -10749),
    (0x2C6F, 0x2C6F, 1, -10783),
    (0x2C70, 0x2C70, 1, -10782),
    (0x2C72, 0x2C75, 3, 1),
    (0x2C7E, 0x2C7F, 1, -10815),
    (0x2C80, 0x2CE2, 2, 1),
    (0x2CEB, 0x2CED, 2, 1),
    (0x2CF2, 0xA640, 31054, 1),
    (0xA642, 0xA66C, 2, 1),
    (0xA680, 0xA69A, 2, 1),
    (0xA722, 0xA72E, 2, 1),
    (0xA732, 0xA76E, 2, 1),
    (0xA779, 0xA77B, 2, 1),
    (0xA77D, 0xA77D, 1, -35332),
    (0xA77E, 0xA786, 2, 1),
    (0xA78B, 0xA78B, 1, 1),
    (0xA78D, 0xA78D, 1, -42280),
    (0xA790, 0xA792, 2, 1),
    (0xA796, 0xA7A8, 2, 1),
    (0xA7AA, 0xA7AA, 1, -42308),
    (0xA7AB, 0xA7AB, 1, -42319),
    (0xA7AC, 0xA7AC, 1, -42315),
    (0xA7AD, 0xA7AD, 1, -42305),
    (0xA7AE, 0xA7AE, 1, -42308),
    (0xA7B0, 0xA7B0, 1, -42258),
    (0xA7B1, 0xA7B1, 1, -42282),
    (0xA7B2, 0xA7B2, 1, -42261),
    (0xA7B3, 0xA7B3, 1, 928),
    (0xA7B4, 0xA7C2, 2, 1),
    (0xA7C4, 0xA7C4, 1, -48),
    (0xA7C5, 0xA7C5, 1, -42307),
    (0xA7C6, 0xA7C6, 1, -35384),
    (0xA7C7, 0xA7C9, 2, 1),
    (0xA7D0, 0xA7D6, 6, 1),
    (0xA7D8, 0xA7F5, 29, 1),
    (0xFF21, 0xFF3A, 1, 32),
    (0x10400, 0x10427, 1, 40),
    (0x104B0, 0x104D3, 1, 40),
    (0x10570, 0x1057A, 1, 39),
    (0x1057C, 0x1058A, 1, 39),
    (0x1058C, 0x10592, 1, 39),
    (0x10594, 0x10595, 1, 39),
    (0x10C80, 0x10CB2, 1, 64),
    (0x118A0, 0x118BF, 1, 32),
    (0x16E40, 0x16E5F, 1, 32),
    (0x1E900, 0x1E921, 1, 34),
];

// Generated from Unicode 15.0.0 DerivedCoreProperties.txt: Cased.
const UNICODE_15_CASED: &[(u32, u32)] = &[
    (0x0041, 0x005A),
    (0x0061, 0x007A),
    (0x00AA, 0x00AA),
    (0x00B5, 0x00B5),
    (0x00BA, 0x00BA),
    (0x00C0, 0x00D6),
    (0x00D8, 0x00F6),
    (0x00F8, 0x01BA),
    (0x01BC, 0x01BF),
    (0x01C4, 0x0293),
    (0x0295, 0x02B8),
    (0x02C0, 0x02C1),
    (0x02E0, 0x02E4),
    (0x0345, 0x0345),
    (0x0370, 0x0373),
    (0x0376, 0x0377),
    (0x037A, 0x037D),
    (0x037F, 0x037F),
    (0x0386, 0x0386),
    (0x0388, 0x038A),
    (0x038C, 0x038C),
    (0x038E, 0x03A1),
    (0x03A3, 0x03F5),
    (0x03F7, 0x0481),
    (0x048A, 0x052F),
    (0x0531, 0x0556),
    (0x0560, 0x0588),
    (0x10A0, 0x10C5),
    (0x10C7, 0x10C7),
    (0x10CD, 0x10CD),
    (0x10D0, 0x10FA),
    (0x10FC, 0x10FF),
    (0x13A0, 0x13F5),
    (0x13F8, 0x13FD),
    (0x1C80, 0x1C88),
    (0x1C90, 0x1CBA),
    (0x1CBD, 0x1CBF),
    (0x1D00, 0x1DBF),
    (0x1E00, 0x1F15),
    (0x1F18, 0x1F1D),
    (0x1F20, 0x1F45),
    (0x1F48, 0x1F4D),
    (0x1F50, 0x1F57),
    (0x1F59, 0x1F59),
    (0x1F5B, 0x1F5B),
    (0x1F5D, 0x1F5D),
    (0x1F5F, 0x1F7D),
    (0x1F80, 0x1FB4),
    (0x1FB6, 0x1FBC),
    (0x1FBE, 0x1FBE),
    (0x1FC2, 0x1FC4),
    (0x1FC6, 0x1FCC),
    (0x1FD0, 0x1FD3),
    (0x1FD6, 0x1FDB),
    (0x1FE0, 0x1FEC),
    (0x1FF2, 0x1FF4),
    (0x1FF6, 0x1FFC),
    (0x2071, 0x2071),
    (0x207F, 0x207F),
    (0x2090, 0x209C),
    (0x2102, 0x2102),
    (0x2107, 0x2107),
    (0x210A, 0x2113),
    (0x2115, 0x2115),
    (0x2119, 0x211D),
    (0x2124, 0x2124),
    (0x2126, 0x2126),
    (0x2128, 0x2128),
    (0x212A, 0x212D),
    (0x212F, 0x2134),
    (0x2139, 0x2139),
    (0x213C, 0x213F),
    (0x2145, 0x2149),
    (0x214E, 0x214E),
    (0x2160, 0x217F),
    (0x2183, 0x2184),
    (0x24B6, 0x24E9),
    (0x2C00, 0x2CE4),
    (0x2CEB, 0x2CEE),
    (0x2CF2, 0x2CF3),
    (0x2D00, 0x2D25),
    (0x2D27, 0x2D27),
    (0x2D2D, 0x2D2D),
    (0xA640, 0xA66D),
    (0xA680, 0xA69D),
    (0xA722, 0xA787),
    (0xA78B, 0xA78E),
    (0xA790, 0xA7CA),
    (0xA7D0, 0xA7D1),
    (0xA7D3, 0xA7D3),
    (0xA7D5, 0xA7D9),
    (0xA7F2, 0xA7F6),
    (0xA7F8, 0xA7FA),
    (0xAB30, 0xAB5A),
    (0xAB5C, 0xAB69),
    (0xAB70, 0xABBF),
    (0xFB00, 0xFB06),
    (0xFB13, 0xFB17),
    (0xFF21, 0xFF3A),
    (0xFF41, 0xFF5A),
    (0x10400, 0x1044F),
    (0x104B0, 0x104D3),
    (0x104D8, 0x104FB),
    (0x10570, 0x1057A),
    (0x1057C, 0x1058A),
    (0x1058C, 0x10592),
    (0x10594, 0x10595),
    (0x10597, 0x105A1),
    (0x105A3, 0x105B1),
    (0x105B3, 0x105B9),
    (0x105BB, 0x105BC),
    (0x10780, 0x10780),
    (0x10783, 0x10785),
    (0x10787, 0x107B0),
    (0x107B2, 0x107BA),
    (0x10C80, 0x10CB2),
    (0x10CC0, 0x10CF2),
    (0x118A0, 0x118DF),
    (0x16E40, 0x16E7F),
    (0x1D400, 0x1D454),
    (0x1D456, 0x1D49C),
    (0x1D49E, 0x1D49F),
    (0x1D4A2, 0x1D4A2),
    (0x1D4A5, 0x1D4A6),
    (0x1D4A9, 0x1D4AC),
    (0x1D4AE, 0x1D4B9),
    (0x1D4BB, 0x1D4BB),
    (0x1D4BD, 0x1D4C3),
    (0x1D4C5, 0x1D505),
    (0x1D507, 0x1D50A),
    (0x1D50D, 0x1D514),
    (0x1D516, 0x1D51C),
    (0x1D51E, 0x1D539),
    (0x1D53B, 0x1D53E),
    (0x1D540, 0x1D544),
    (0x1D546, 0x1D546),
    (0x1D54A, 0x1D550),
    (0x1D552, 0x1D6A5),
    (0x1D6A8, 0x1D6C0),
    (0x1D6C2, 0x1D6DA),
    (0x1D6DC, 0x1D6FA),
    (0x1D6FC, 0x1D714),
    (0x1D716, 0x1D734),
    (0x1D736, 0x1D74E),
    (0x1D750, 0x1D76E),
    (0x1D770, 0x1D788),
    (0x1D78A, 0x1D7A8),
    (0x1D7AA, 0x1D7C2),
    (0x1D7C4, 0x1D7CB),
    (0x1DF00, 0x1DF09),
    (0x1DF0B, 0x1DF1E),
    (0x1DF25, 0x1DF2A),
    (0x1E030, 0x1E06D),
    (0x1E900, 0x1E943),
    (0x1F130, 0x1F149),
    (0x1F150, 0x1F169),
    (0x1F170, 0x1F189),
];

// Generated from Unicode 15.0.0 DerivedCoreProperties.txt: Case_Ignorable.
const UNICODE_15_CASE_IGNORABLE: &[(u32, u32)] = &[
    (0x0027, 0x0027),
    (0x002E, 0x002E),
    (0x003A, 0x003A),
    (0x005E, 0x005E),
    (0x0060, 0x0060),
    (0x00A8, 0x00A8),
    (0x00AD, 0x00AD),
    (0x00AF, 0x00AF),
    (0x00B4, 0x00B4),
    (0x00B7, 0x00B8),
    (0x02B0, 0x036F),
    (0x0374, 0x0375),
    (0x037A, 0x037A),
    (0x0384, 0x0385),
    (0x0387, 0x0387),
    (0x0483, 0x0489),
    (0x0559, 0x0559),
    (0x055F, 0x055F),
    (0x0591, 0x05BD),
    (0x05BF, 0x05BF),
    (0x05C1, 0x05C2),
    (0x05C4, 0x05C5),
    (0x05C7, 0x05C7),
    (0x05F4, 0x05F4),
    (0x0600, 0x0605),
    (0x0610, 0x061A),
    (0x061C, 0x061C),
    (0x0640, 0x0640),
    (0x064B, 0x065F),
    (0x0670, 0x0670),
    (0x06D6, 0x06DD),
    (0x06DF, 0x06E8),
    (0x06EA, 0x06ED),
    (0x070F, 0x070F),
    (0x0711, 0x0711),
    (0x0730, 0x074A),
    (0x07A6, 0x07B0),
    (0x07EB, 0x07F5),
    (0x07FA, 0x07FA),
    (0x07FD, 0x07FD),
    (0x0816, 0x082D),
    (0x0859, 0x085B),
    (0x0888, 0x0888),
    (0x0890, 0x0891),
    (0x0898, 0x089F),
    (0x08C9, 0x0902),
    (0x093A, 0x093A),
    (0x093C, 0x093C),
    (0x0941, 0x0948),
    (0x094D, 0x094D),
    (0x0951, 0x0957),
    (0x0962, 0x0963),
    (0x0971, 0x0971),
    (0x0981, 0x0981),
    (0x09BC, 0x09BC),
    (0x09C1, 0x09C4),
    (0x09CD, 0x09CD),
    (0x09E2, 0x09E3),
    (0x09FE, 0x09FE),
    (0x0A01, 0x0A02),
    (0x0A3C, 0x0A3C),
    (0x0A41, 0x0A42),
    (0x0A47, 0x0A48),
    (0x0A4B, 0x0A4D),
    (0x0A51, 0x0A51),
    (0x0A70, 0x0A71),
    (0x0A75, 0x0A75),
    (0x0A81, 0x0A82),
    (0x0ABC, 0x0ABC),
    (0x0AC1, 0x0AC5),
    (0x0AC7, 0x0AC8),
    (0x0ACD, 0x0ACD),
    (0x0AE2, 0x0AE3),
    (0x0AFA, 0x0AFF),
    (0x0B01, 0x0B01),
    (0x0B3C, 0x0B3C),
    (0x0B3F, 0x0B3F),
    (0x0B41, 0x0B44),
    (0x0B4D, 0x0B4D),
    (0x0B55, 0x0B56),
    (0x0B62, 0x0B63),
    (0x0B82, 0x0B82),
    (0x0BC0, 0x0BC0),
    (0x0BCD, 0x0BCD),
    (0x0C00, 0x0C00),
    (0x0C04, 0x0C04),
    (0x0C3C, 0x0C3C),
    (0x0C3E, 0x0C40),
    (0x0C46, 0x0C48),
    (0x0C4A, 0x0C4D),
    (0x0C55, 0x0C56),
    (0x0C62, 0x0C63),
    (0x0C81, 0x0C81),
    (0x0CBC, 0x0CBC),
    (0x0CBF, 0x0CBF),
    (0x0CC6, 0x0CC6),
    (0x0CCC, 0x0CCD),
    (0x0CE2, 0x0CE3),
    (0x0D00, 0x0D01),
    (0x0D3B, 0x0D3C),
    (0x0D41, 0x0D44),
    (0x0D4D, 0x0D4D),
    (0x0D62, 0x0D63),
    (0x0D81, 0x0D81),
    (0x0DCA, 0x0DCA),
    (0x0DD2, 0x0DD4),
    (0x0DD6, 0x0DD6),
    (0x0E31, 0x0E31),
    (0x0E34, 0x0E3A),
    (0x0E46, 0x0E4E),
    (0x0EB1, 0x0EB1),
    (0x0EB4, 0x0EBC),
    (0x0EC6, 0x0EC6),
    (0x0EC8, 0x0ECE),
    (0x0F18, 0x0F19),
    (0x0F35, 0x0F35),
    (0x0F37, 0x0F37),
    (0x0F39, 0x0F39),
    (0x0F71, 0x0F7E),
    (0x0F80, 0x0F84),
    (0x0F86, 0x0F87),
    (0x0F8D, 0x0F97),
    (0x0F99, 0x0FBC),
    (0x0FC6, 0x0FC6),
    (0x102D, 0x1030),
    (0x1032, 0x1037),
    (0x1039, 0x103A),
    (0x103D, 0x103E),
    (0x1058, 0x1059),
    (0x105E, 0x1060),
    (0x1071, 0x1074),
    (0x1082, 0x1082),
    (0x1085, 0x1086),
    (0x108D, 0x108D),
    (0x109D, 0x109D),
    (0x10FC, 0x10FC),
    (0x135D, 0x135F),
    (0x1712, 0x1714),
    (0x1732, 0x1733),
    (0x1752, 0x1753),
    (0x1772, 0x1773),
    (0x17B4, 0x17B5),
    (0x17B7, 0x17BD),
    (0x17C6, 0x17C6),
    (0x17C9, 0x17D3),
    (0x17D7, 0x17D7),
    (0x17DD, 0x17DD),
    (0x180B, 0x180F),
    (0x1843, 0x1843),
    (0x1885, 0x1886),
    (0x18A9, 0x18A9),
    (0x1920, 0x1922),
    (0x1927, 0x1928),
    (0x1932, 0x1932),
    (0x1939, 0x193B),
    (0x1A17, 0x1A18),
    (0x1A1B, 0x1A1B),
    (0x1A56, 0x1A56),
    (0x1A58, 0x1A5E),
    (0x1A60, 0x1A60),
    (0x1A62, 0x1A62),
    (0x1A65, 0x1A6C),
    (0x1A73, 0x1A7C),
    (0x1A7F, 0x1A7F),
    (0x1AA7, 0x1AA7),
    (0x1AB0, 0x1ACE),
    (0x1B00, 0x1B03),
    (0x1B34, 0x1B34),
    (0x1B36, 0x1B3A),
    (0x1B3C, 0x1B3C),
    (0x1B42, 0x1B42),
    (0x1B6B, 0x1B73),
    (0x1B80, 0x1B81),
    (0x1BA2, 0x1BA5),
    (0x1BA8, 0x1BA9),
    (0x1BAB, 0x1BAD),
    (0x1BE6, 0x1BE6),
    (0x1BE8, 0x1BE9),
    (0x1BED, 0x1BED),
    (0x1BEF, 0x1BF1),
    (0x1C2C, 0x1C33),
    (0x1C36, 0x1C37),
    (0x1C78, 0x1C7D),
    (0x1CD0, 0x1CD2),
    (0x1CD4, 0x1CE0),
    (0x1CE2, 0x1CE8),
    (0x1CED, 0x1CED),
    (0x1CF4, 0x1CF4),
    (0x1CF8, 0x1CF9),
    (0x1D2C, 0x1D6A),
    (0x1D78, 0x1D78),
    (0x1D9B, 0x1DFF),
    (0x1FBD, 0x1FBD),
    (0x1FBF, 0x1FC1),
    (0x1FCD, 0x1FCF),
    (0x1FDD, 0x1FDF),
    (0x1FED, 0x1FEF),
    (0x1FFD, 0x1FFE),
    (0x200B, 0x200F),
    (0x2018, 0x2019),
    (0x2024, 0x2024),
    (0x2027, 0x2027),
    (0x202A, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0x2071, 0x2071),
    (0x207F, 0x207F),
    (0x2090, 0x209C),
    (0x20D0, 0x20F0),
    (0x2C7C, 0x2C7D),
    (0x2CEF, 0x2CF1),
    (0x2D6F, 0x2D6F),
    (0x2D7F, 0x2D7F),
    (0x2DE0, 0x2DFF),
    (0x2E2F, 0x2E2F),
    (0x3005, 0x3005),
    (0x302A, 0x302D),
    (0x3031, 0x3035),
    (0x303B, 0x303B),
    (0x3099, 0x309E),
    (0x30FC, 0x30FE),
    (0xA015, 0xA015),
    (0xA4F8, 0xA4FD),
    (0xA60C, 0xA60C),
    (0xA66F, 0xA672),
    (0xA674, 0xA67D),
    (0xA67F, 0xA67F),
    (0xA69C, 0xA69F),
    (0xA6F0, 0xA6F1),
    (0xA700, 0xA721),
    (0xA770, 0xA770),
    (0xA788, 0xA78A),
    (0xA7F2, 0xA7F4),
    (0xA7F8, 0xA7F9),
    (0xA802, 0xA802),
    (0xA806, 0xA806),
    (0xA80B, 0xA80B),
    (0xA825, 0xA826),
    (0xA82C, 0xA82C),
    (0xA8C4, 0xA8C5),
    (0xA8E0, 0xA8F1),
    (0xA8FF, 0xA8FF),
    (0xA926, 0xA92D),
    (0xA947, 0xA951),
    (0xA980, 0xA982),
    (0xA9B3, 0xA9B3),
    (0xA9B6, 0xA9B9),
    (0xA9BC, 0xA9BD),
    (0xA9CF, 0xA9CF),
    (0xA9E5, 0xA9E6),
    (0xAA29, 0xAA2E),
    (0xAA31, 0xAA32),
    (0xAA35, 0xAA36),
    (0xAA43, 0xAA43),
    (0xAA4C, 0xAA4C),
    (0xAA70, 0xAA70),
    (0xAA7C, 0xAA7C),
    (0xAAB0, 0xAAB0),
    (0xAAB2, 0xAAB4),
    (0xAAB7, 0xAAB8),
    (0xAABE, 0xAABF),
    (0xAAC1, 0xAAC1),
    (0xAADD, 0xAADD),
    (0xAAEC, 0xAAED),
    (0xAAF3, 0xAAF4),
    (0xAAF6, 0xAAF6),
    (0xAB5B, 0xAB5F),
    (0xAB69, 0xAB6B),
    (0xABE5, 0xABE5),
    (0xABE8, 0xABE8),
    (0xABED, 0xABED),
    (0xFB1E, 0xFB1E),
    (0xFBB2, 0xFBC2),
    (0xFE00, 0xFE0F),
    (0xFE13, 0xFE13),
    (0xFE20, 0xFE2F),
    (0xFE52, 0xFE52),
    (0xFE55, 0xFE55),
    (0xFEFF, 0xFEFF),
    (0xFF07, 0xFF07),
    (0xFF0E, 0xFF0E),
    (0xFF1A, 0xFF1A),
    (0xFF3E, 0xFF3E),
    (0xFF40, 0xFF40),
    (0xFF70, 0xFF70),
    (0xFF9E, 0xFF9F),
    (0xFFE3, 0xFFE3),
    (0xFFF9, 0xFFFB),
    (0x101FD, 0x101FD),
    (0x102E0, 0x102E0),
    (0x10376, 0x1037A),
    (0x10780, 0x10785),
    (0x10787, 0x107B0),
    (0x107B2, 0x107BA),
    (0x10A01, 0x10A03),
    (0x10A05, 0x10A06),
    (0x10A0C, 0x10A0F),
    (0x10A38, 0x10A3A),
    (0x10A3F, 0x10A3F),
    (0x10AE5, 0x10AE6),
    (0x10D24, 0x10D27),
    (0x10EAB, 0x10EAC),
    (0x10EFD, 0x10EFF),
    (0x10F46, 0x10F50),
    (0x10F82, 0x10F85),
    (0x11001, 0x11001),
    (0x11038, 0x11046),
    (0x11070, 0x11070),
    (0x11073, 0x11074),
    (0x1107F, 0x11081),
    (0x110B3, 0x110B6),
    (0x110B9, 0x110BA),
    (0x110BD, 0x110BD),
    (0x110C2, 0x110C2),
    (0x110CD, 0x110CD),
    (0x11100, 0x11102),
    (0x11127, 0x1112B),
    (0x1112D, 0x11134),
    (0x11173, 0x11173),
    (0x11180, 0x11181),
    (0x111B6, 0x111BE),
    (0x111C9, 0x111CC),
    (0x111CF, 0x111CF),
    (0x1122F, 0x11231),
    (0x11234, 0x11234),
    (0x11236, 0x11237),
    (0x1123E, 0x1123E),
    (0x11241, 0x11241),
    (0x112DF, 0x112DF),
    (0x112E3, 0x112EA),
    (0x11300, 0x11301),
    (0x1133B, 0x1133C),
    (0x11340, 0x11340),
    (0x11366, 0x1136C),
    (0x11370, 0x11374),
    (0x11438, 0x1143F),
    (0x11442, 0x11444),
    (0x11446, 0x11446),
    (0x1145E, 0x1145E),
    (0x114B3, 0x114B8),
    (0x114BA, 0x114BA),
    (0x114BF, 0x114C0),
    (0x114C2, 0x114C3),
    (0x115B2, 0x115B5),
    (0x115BC, 0x115BD),
    (0x115BF, 0x115C0),
    (0x115DC, 0x115DD),
    (0x11633, 0x1163A),
    (0x1163D, 0x1163D),
    (0x1163F, 0x11640),
    (0x116AB, 0x116AB),
    (0x116AD, 0x116AD),
    (0x116B0, 0x116B5),
    (0x116B7, 0x116B7),
    (0x1171D, 0x1171F),
    (0x11722, 0x11725),
    (0x11727, 0x1172B),
    (0x1182F, 0x11837),
    (0x11839, 0x1183A),
    (0x1193B, 0x1193C),
    (0x1193E, 0x1193E),
    (0x11943, 0x11943),
    (0x119D4, 0x119D7),
    (0x119DA, 0x119DB),
    (0x119E0, 0x119E0),
    (0x11A01, 0x11A0A),
    (0x11A33, 0x11A38),
    (0x11A3B, 0x11A3E),
    (0x11A47, 0x11A47),
    (0x11A51, 0x11A56),
    (0x11A59, 0x11A5B),
    (0x11A8A, 0x11A96),
    (0x11A98, 0x11A99),
    (0x11C30, 0x11C36),
    (0x11C38, 0x11C3D),
    (0x11C3F, 0x11C3F),
    (0x11C92, 0x11CA7),
    (0x11CAA, 0x11CB0),
    (0x11CB2, 0x11CB3),
    (0x11CB5, 0x11CB6),
    (0x11D31, 0x11D36),
    (0x11D3A, 0x11D3A),
    (0x11D3C, 0x11D3D),
    (0x11D3F, 0x11D45),
    (0x11D47, 0x11D47),
    (0x11D90, 0x11D91),
    (0x11D95, 0x11D95),
    (0x11D97, 0x11D97),
    (0x11EF3, 0x11EF4),
    (0x11F00, 0x11F01),
    (0x11F36, 0x11F3A),
    (0x11F40, 0x11F40),
    (0x11F42, 0x11F42),
    (0x13430, 0x13440),
    (0x13447, 0x13455),
    (0x16AF0, 0x16AF4),
    (0x16B30, 0x16B36),
    (0x16B40, 0x16B43),
    (0x16F4F, 0x16F4F),
    (0x16F8F, 0x16F9F),
    (0x16FE0, 0x16FE1),
    (0x16FE3, 0x16FE4),
    (0x1AFF0, 0x1AFF3),
    (0x1AFF5, 0x1AFFB),
    (0x1AFFD, 0x1AFFE),
    (0x1BC9D, 0x1BC9E),
    (0x1BCA0, 0x1BCA3),
    (0x1CF00, 0x1CF2D),
    (0x1CF30, 0x1CF46),
    (0x1D167, 0x1D169),
    (0x1D173, 0x1D182),
    (0x1D185, 0x1D18B),
    (0x1D1AA, 0x1D1AD),
    (0x1D242, 0x1D244),
    (0x1DA00, 0x1DA36),
    (0x1DA3B, 0x1DA6C),
    (0x1DA75, 0x1DA75),
    (0x1DA84, 0x1DA84),
    (0x1DA9B, 0x1DA9F),
    (0x1DAA1, 0x1DAAF),
    (0x1E000, 0x1E006),
    (0x1E008, 0x1E018),
    (0x1E01B, 0x1E021),
    (0x1E023, 0x1E024),
    (0x1E026, 0x1E02A),
    (0x1E030, 0x1E06D),
    (0x1E08F, 0x1E08F),
    (0x1E130, 0x1E13D),
    (0x1E2AE, 0x1E2AE),
    (0x1E2EC, 0x1E2EF),
    (0x1E4EB, 0x1E4EF),
    (0x1E8D0, 0x1E8D6),
    (0x1E944, 0x1E94B),
    (0x1F3FB, 0x1F3FF),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
    (0xE0100, 0xE01EF),
];
