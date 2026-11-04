//! UI-independent spreadsheet model, formula evaluator and portable persistence codec.
//!
//! The engine deliberately owns no GUI state. Cell coordinates are zero-based
//! internally; public A1 helpers use the familiar spreadsheet notation.

use std::collections::{BTreeMap, HashSet};
use std::io;

const WORKBOOK_MAGIC: &[u8; 8] = b"LOUSXLS1";
const WORKBOOK_VERSION: u32 = 1;
const MAX_SHEETS: usize = 16_384;
const MAX_CELLS: usize = 1_000_000;
const MAX_STRING: usize = 16 * 1024 * 1024;
const MAX_ROW: u32 = 1_048_575;
const MAX_COLUMN: u32 = 16_383;

pub const fn engine_name() -> &'static str {
    "Spreadsheet Engine"
}

#[derive(Debug, Clone, PartialEq)]
pub enum CellValue {
    Blank,
    Number(f64),
    Text(String),
    Boolean(bool),
    Error(FormulaError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormulaError {
    Parse,
    Reference,
    DivisionByZero,
    CircularReference,
    Type,
    Number,
}

#[derive(Debug, Clone, PartialEq)]
enum CellInput {
    Value(CellValue),
    Formula(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    input: CellInput,
}

impl Cell {
    pub fn is_formula(&self) -> bool {
        matches!(self.input, CellInput::Formula(_))
    }
    pub fn formula(&self) -> Option<&str> {
        match &self.input {
            CellInput::Formula(value) => Some(value),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellAddress {
    row: u32,
    column: u32,
}

impl CellAddress {
    pub fn new(row: u32, column: u32) -> io::Result<Self> {
        if row > MAX_ROW || column > MAX_COLUMN {
            return Err(invalid_input(
                "cell address is outside supported worksheet limits",
            ));
        }
        Ok(Self { row, column })
    }
    pub fn row(self) -> u32 {
        self.row
    }
    pub fn column(self) -> u32 {
        self.column
    }
    pub fn parse_a1(value: &str) -> io::Result<Self> {
        let split = value
            .bytes()
            .position(|b| b.is_ascii_digit())
            .ok_or_else(|| invalid_input("invalid A1 cell address"))?;
        if split == 0 || !value.is_ascii() {
            return Err(invalid_input("invalid A1 cell address"));
        }
        let mut column: u64 = 0;
        for byte in value[..split].bytes() {
            let upper = byte.to_ascii_uppercase();
            if !upper.is_ascii_uppercase() {
                return Err(invalid_input("invalid A1 column"));
            }
            column = column
                .checked_mul(26)
                .and_then(|n| n.checked_add((upper - b'A' + 1) as u64))
                .ok_or_else(|| invalid_input("A1 column overflow"))?;
        }
        let row_text = &value[split..];
        if row_text.is_empty()
            || !row_text.bytes().all(|b| b.is_ascii_digit())
            || row_text.starts_with('0')
        {
            return Err(invalid_input("invalid A1 row"));
        }
        let row: u32 = row_text
            .parse()
            .map_err(|_| invalid_input("A1 row overflow"))?;
        if row == 0 {
            return Err(invalid_input("A1 rows start at one"));
        }
        Self::new(
            row - 1,
            u32::try_from(column - 1).map_err(|_| invalid_input("A1 column overflow"))?,
        )
    }
    pub fn to_a1(self) -> String {
        let mut n = self.column + 1;
        let mut letters = Vec::new();
        while n > 0 {
            let digit = ((n - 1) % 26) as u8;
            letters.push((b'A' + digit) as char);
            n = (n - 1) / 26;
        }
        letters.iter().rev().collect::<String>() + &(self.row + 1).to_string()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Worksheet {
    id: u64,
    name: String,
    cells: BTreeMap<CellAddress, Cell>,
}

impl Worksheet {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn cell(&self, address: CellAddress) -> Option<&Cell> {
        self.cells.get(&address)
    }
    pub fn populated_cells(&self) -> impl Iterator<Item = (CellAddress, &Cell)> {
        self.cells.iter().map(|(address, cell)| (*address, cell))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Workbook {
    next_sheet_id: u64,
    sheets: Vec<Worksheet>,
}

impl Workbook {
    pub fn new() -> Self {
        Self {
            next_sheet_id: 2,
            sheets: vec![Worksheet {
                id: 1,
                name: "Sheet1".into(),
                cells: BTreeMap::new(),
            }],
        }
    }
    pub fn sheets(&self) -> &[Worksheet] {
        &self.sheets
    }
    pub fn sheet(&self, id: u64) -> Option<&Worksheet> {
        self.sheets.iter().find(|sheet| sheet.id == id)
    }
    pub fn add_sheet(&mut self, name: impl Into<String>) -> io::Result<u64> {
        let name = name.into();
        validate_sheet_name(&name)?;
        if self
            .sheets
            .iter()
            .any(|sheet| sheet.name.eq_ignore_ascii_case(&name))
        {
            return Err(invalid_input(
                "worksheet names must be unique (case-insensitive)",
            ));
        }
        if self.sheets.len() >= MAX_SHEETS {
            return Err(invalid_input("worksheet limit reached"));
        }
        let id = self.next_sheet_id;
        self.next_sheet_id = id
            .checked_add(1)
            .ok_or_else(|| invalid_input("worksheet ID exhausted"))?;
        self.sheets.push(Worksheet {
            id,
            name,
            cells: BTreeMap::new(),
        });
        Ok(id)
    }
    pub fn rename_sheet(&mut self, id: u64, name: impl Into<String>) -> io::Result<()> {
        let name = name.into();
        validate_sheet_name(&name)?;
        if self
            .sheets
            .iter()
            .any(|sheet| sheet.id != id && sheet.name.eq_ignore_ascii_case(&name))
        {
            return Err(invalid_input(
                "worksheet names must be unique (case-insensitive)",
            ));
        }
        let sheet = self
            .sheets
            .iter_mut()
            .find(|sheet| sheet.id == id)
            .ok_or_else(|| invalid_input("unknown worksheet"))?;
        sheet.name = name;
        Ok(())
    }
    pub fn remove_sheet(&mut self, id: u64) -> io::Result<()> {
        if self.sheets.len() <= 1 {
            return Err(invalid_input(
                "a workbook must contain at least one worksheet",
            ));
        }
        let index = self
            .sheets
            .iter()
            .position(|sheet| sheet.id == id)
            .ok_or_else(|| invalid_input("unknown worksheet"))?;
        self.sheets.remove(index);
        Ok(())
    }
    pub fn set_number(
        &mut self,
        sheet_id: u64,
        address: CellAddress,
        value: f64,
    ) -> io::Result<()> {
        if !value.is_finite() {
            return Err(invalid_input("cell numbers must be finite"));
        }
        self.set_input(
            sheet_id,
            address,
            CellInput::Value(CellValue::Number(value)),
        )
    }
    pub fn set_text(
        &mut self,
        sheet_id: u64,
        address: CellAddress,
        value: impl Into<String>,
    ) -> io::Result<()> {
        let value = value.into();
        validate_string(&value)?;
        self.set_input(sheet_id, address, CellInput::Value(CellValue::Text(value)))
    }
    pub fn set_boolean(
        &mut self,
        sheet_id: u64,
        address: CellAddress,
        value: bool,
    ) -> io::Result<()> {
        self.set_input(
            sheet_id,
            address,
            CellInput::Value(CellValue::Boolean(value)),
        )
    }
    pub fn set_formula(
        &mut self,
        sheet_id: u64,
        address: CellAddress,
        formula: impl Into<String>,
    ) -> io::Result<()> {
        let formula = formula.into();
        validate_string(&formula)?;
        if !formula.starts_with('=') || formula.len() < 2 {
            return Err(invalid_input(
                "formulas must start with '=' and contain an expression",
            ));
        }
        self.set_input(sheet_id, address, CellInput::Formula(formula))
    }
    pub fn clear_cell(&mut self, sheet_id: u64, address: CellAddress) -> io::Result<()> {
        let sheet = self
            .sheets
            .iter_mut()
            .find(|sheet| sheet.id == sheet_id)
            .ok_or_else(|| invalid_input("unknown worksheet"))?;
        sheet.cells.remove(&address);
        Ok(())
    }
    fn set_input(
        &mut self,
        sheet_id: u64,
        address: CellAddress,
        input: CellInput,
    ) -> io::Result<()> {
        let sheet = self
            .sheets
            .iter_mut()
            .find(|sheet| sheet.id == sheet_id)
            .ok_or_else(|| invalid_input("unknown worksheet"))?;
        if !sheet.cells.contains_key(&address) && sheet.cells.len() >= MAX_CELLS {
            return Err(invalid_input("worksheet cell limit reached"));
        }
        sheet.cells.insert(address, Cell { input });
        Ok(())
    }
    /// Evaluate a cell and all formula dependencies. Results are computed on demand,
    /// so edits cannot leave stale cached values behind.
    pub fn value(&self, sheet_id: u64, address: CellAddress) -> CellValue {
        let mut visiting = HashSet::new();
        self.evaluate(sheet_id, address, &mut visiting)
    }
    fn evaluate(
        &self,
        sheet_id: u64,
        address: CellAddress,
        visiting: &mut HashSet<(u64, CellAddress)>,
    ) -> CellValue {
        let key = (sheet_id, address);
        let Some(sheet) = self.sheet(sheet_id) else {
            return CellValue::Error(FormulaError::Reference);
        };
        let Some(cell) = sheet.cells.get(&address) else {
            return CellValue::Blank;
        };
        match &cell.input {
            CellInput::Value(value) => value.clone(),
            CellInput::Formula(formula) => {
                if !visiting.insert(key) {
                    return CellValue::Error(FormulaError::CircularReference);
                }
                let result = Parser::new(&formula[1..], self, sheet_id, visiting).parse();
                visiting.remove(&key);
                match result {
                    Ok(value) if value.is_finite() => CellValue::Number(value),
                    Ok(_) => CellValue::Error(FormulaError::Number),
                    Err(error) => CellValue::Error(error),
                }
            }
        }
    }
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        encode_workbook(self)
    }
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        decode_workbook(bytes)
    }
}

impl Default for Workbook {
    fn default() -> Self {
        Self::new()
    }
}

struct Parser<'a> {
    input: &'a [u8],
    cursor: usize,
    workbook: &'a Workbook,
    sheet_id: u64,
    visiting: &'a mut HashSet<(u64, CellAddress)>,
}
impl<'a> Parser<'a> {
    fn new(
        input: &'a str,
        workbook: &'a Workbook,
        sheet_id: u64,
        visiting: &'a mut HashSet<(u64, CellAddress)>,
    ) -> Self {
        Self {
            input: input.as_bytes(),
            cursor: 0,
            workbook,
            sheet_id,
            visiting,
        }
    }
    fn parse(mut self) -> Result<f64, FormulaError> {
        let value = self.expression()?;
        self.whitespace();
        if self.cursor != self.input.len() {
            return Err(FormulaError::Parse);
        }
        Ok(value)
    }
    fn expression(&mut self) -> Result<f64, FormulaError> {
        let mut value = self.term()?;
        loop {
            self.whitespace();
            match self.peek() {
                Some(b'+') => {
                    self.cursor += 1;
                    value += self.term()?;
                }
                Some(b'-') => {
                    self.cursor += 1;
                    value -= self.term()?;
                }
                _ => return Ok(value),
            }
        }
    }
    fn term(&mut self) -> Result<f64, FormulaError> {
        let mut value = self.factor()?;
        loop {
            self.whitespace();
            match self.peek() {
                Some(b'*') => {
                    self.cursor += 1;
                    value *= self.factor()?;
                }
                Some(b'/') => {
                    self.cursor += 1;
                    let divisor = self.factor()?;
                    if divisor == 0.0 {
                        return Err(FormulaError::DivisionByZero);
                    }
                    value /= divisor;
                }
                _ => return Ok(value),
            }
        }
    }
    fn factor(&mut self) -> Result<f64, FormulaError> {
        self.whitespace();
        match self.peek() {
            Some(b'+') => {
                self.cursor += 1;
                self.factor()
            }
            Some(b'-') => {
                self.cursor += 1;
                Ok(-self.factor()?)
            }
            Some(b'(') => {
                self.cursor += 1;
                let value = self.expression()?;
                self.whitespace();
                if self.peek() != Some(b')') {
                    return Err(FormulaError::Parse);
                }
                self.cursor += 1;
                Ok(value)
            }
            Some(byte) if byte.is_ascii_digit() || byte == b'.' => self.number(),
            Some(byte) if byte.is_ascii_alphabetic() => self.reference(),
            _ => Err(FormulaError::Parse),
        }
    }
    fn number(&mut self) -> Result<f64, FormulaError> {
        let start = self.cursor;
        while matches!(self.peek(), Some(b'0'..=b'9' | b'.')) {
            self.cursor += 1;
        }
        std::str::from_utf8(&self.input[start..self.cursor])
            .ok()
            .and_then(|text| text.parse::<f64>().ok())
            .filter(|number| number.is_finite())
            .ok_or(FormulaError::Parse)
    }
    fn reference(&mut self) -> Result<f64, FormulaError> {
        let start = self.cursor;
        while matches!(self.peek(), Some(b'A'..=b'Z' | b'a'..=b'z')) {
            self.cursor += 1;
        }
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.cursor += 1;
        }
        let text = std::str::from_utf8(&self.input[start..self.cursor])
            .map_err(|_| FormulaError::Parse)?;
        let address = CellAddress::parse_a1(text).map_err(|_| FormulaError::Reference)?;
        match self
            .workbook
            .evaluate(self.sheet_id, address, self.visiting)
        {
            CellValue::Number(value) => Ok(value),
            CellValue::Blank => Ok(0.0),
            CellValue::Error(error) => Err(error),
            CellValue::Text(_) | CellValue::Boolean(_) => Err(FormulaError::Type),
        }
    }
    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.cursor += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.input.get(self.cursor).copied()
    }
}

fn encode_workbook(workbook: &Workbook) -> io::Result<Vec<u8>> {
    if workbook.sheets.is_empty() || workbook.sheets.len() > MAX_SHEETS {
        return Err(invalid_input("invalid worksheet count"));
    }
    let mut out = Vec::new();
    out.extend_from_slice(WORKBOOK_MAGIC);
    put_u32(&mut out, WORKBOOK_VERSION);
    put_u64(&mut out, workbook.next_sheet_id);
    put_u32(&mut out, workbook.sheets.len() as u32);
    let mut total_cells = 0usize;
    let mut ids = HashSet::new();
    for sheet in &workbook.sheets {
        validate_sheet_name(&sheet.name)?;
        if sheet.id == 0 || !ids.insert(sheet.id) {
            return Err(invalid_input("duplicate or zero worksheet ID"));
        }
        put_u64(&mut out, sheet.id);
        put_string(&mut out, &sheet.name)?;
        put_u32(&mut out, sheet.cells.len() as u32);
        total_cells = total_cells
            .checked_add(sheet.cells.len())
            .ok_or_else(|| invalid_input("cell count overflow"))?;
        if total_cells > MAX_CELLS {
            return Err(invalid_input("workbook cell limit reached"));
        }
        for (address, cell) in &sheet.cells {
            put_u32(&mut out, address.row);
            put_u32(&mut out, address.column);
            match &cell.input {
                CellInput::Value(CellValue::Blank) => out.push(0),
                CellInput::Value(CellValue::Number(value)) if value.is_finite() => {
                    out.push(1);
                    out.extend_from_slice(&value.to_le_bytes());
                }
                CellInput::Value(CellValue::Text(value)) => {
                    out.push(2);
                    put_string(&mut out, value)?;
                }
                CellInput::Value(CellValue::Boolean(value)) => {
                    out.push(3);
                    out.push(u8::from(*value));
                }
                CellInput::Formula(value) => {
                    out.push(4);
                    put_string(&mut out, value)?;
                }
                _ => return Err(invalid_input("invalid stored cell value")),
            }
        }
    }
    if out.len() > MAX_STRING * 4 {
        return Err(invalid_input("workbook encoding exceeds size limit"));
    }
    Ok(out)
}
fn decode_workbook(bytes: &[u8]) -> io::Result<Workbook> {
    let mut input = Reader { bytes, offset: 0 };
    if input.take(8)? != WORKBOOK_MAGIC {
        return Err(invalid_data("invalid workbook signature"));
    }
    if input.u32()? != WORKBOOK_VERSION {
        return Err(invalid_data("unsupported workbook version"));
    }
    let next_sheet_id = input.u64()?;
    let count = input.u32()? as usize;
    if count == 0 || count > MAX_SHEETS || next_sheet_id == 0 {
        return Err(invalid_data("invalid workbook metadata"));
    }
    let mut sheets = Vec::with_capacity(count);
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    let mut total_cells = 0usize;
    for _ in 0..count {
        let id = input.u64()?;
        let name = input.string()?;
        validate_sheet_name(&name).map_err(|_| invalid_data("invalid worksheet name"))?;
        if id == 0 || !ids.insert(id) || !names.insert(name.to_lowercase()) {
            return Err(invalid_data("duplicate worksheet ID or name"));
        }
        let cell_count = input.u32()? as usize;
        total_cells = total_cells
            .checked_add(cell_count)
            .ok_or_else(|| invalid_data("cell count overflow"))?;
        if total_cells > MAX_CELLS {
            return Err(invalid_data("workbook cell limit exceeded"));
        }
        let mut cells = BTreeMap::new();
        for _ in 0..cell_count {
            let address = CellAddress::new(input.u32()?, input.u32()?)
                .map_err(|_| invalid_data("invalid cell coordinate"))?;
            let tag = input.byte()?;
            let value = match tag {
                0 => CellInput::Value(CellValue::Blank),
                1 => {
                    let number = f64::from_le_bytes(input.take(8)?.try_into().unwrap());
                    if !number.is_finite() {
                        return Err(invalid_data("non-finite cell number"));
                    }
                    CellInput::Value(CellValue::Number(number))
                }
                2 => CellInput::Value(CellValue::Text(input.string()?)),
                3 => match input.byte()? {
                    0 => CellInput::Value(CellValue::Boolean(false)),
                    1 => CellInput::Value(CellValue::Boolean(true)),
                    _ => return Err(invalid_data("invalid boolean")),
                },
                4 => {
                    let formula = input.string()?;
                    if !formula.starts_with('=') || formula.len() < 2 {
                        return Err(invalid_data("invalid formula"));
                    }
                    CellInput::Formula(formula)
                }
                _ => return Err(invalid_data("unknown cell encoding")),
            };
            if cells.insert(address, Cell { input: value }).is_some() {
                return Err(invalid_data("duplicate cell coordinate"));
            }
        }
        sheets.push(Worksheet { id, name, cells });
    }
    if input.offset != bytes.len() {
        return Err(invalid_data("trailing workbook data"));
    }
    if ids.iter().any(|id| *id >= next_sheet_id) {
        return Err(invalid_data("next worksheet ID is not monotonic"));
    }
    Ok(Workbook {
        next_sheet_id,
        sheets,
    })
}
struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> io::Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or_else(|| invalid_data("workbook offset overflow"))?;
        let result = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| invalid_data("truncated workbook"))?;
        self.offset = end;
        Ok(result)
    }
    fn byte(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string(&mut self) -> io::Result<String> {
        let len = self.u32()? as usize;
        if len > MAX_STRING {
            return Err(invalid_data("workbook string too large"));
        }
        String::from_utf8(self.take(len)?.to_vec())
            .map_err(|_| invalid_data("invalid workbook UTF-8"))
    }
}
fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_string(out: &mut Vec<u8>, value: &str) -> io::Result<()> {
    validate_string(value)?;
    let len = u32::try_from(value.len()).map_err(|_| invalid_input("string too large"))?;
    put_u32(out, len);
    out.extend_from_slice(value.as_bytes());
    Ok(())
}
fn validate_string(value: &str) -> io::Result<()> {
    if value.len() > MAX_STRING {
        Err(invalid_input("string exceeds 16 MiB"))
    } else {
        Ok(())
    }
}
fn validate_sheet_name(name: &str) -> io::Result<()> {
    if name.trim().is_empty()
        || name.len() > 128
        || name
            .chars()
            .any(|c| matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\'))
    {
        Err(invalid_input(
            "worksheet name is empty, too long, or contains a reserved character",
        ))
    } else {
        Ok(())
    }
}
fn invalid_input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn engine_has_a_name() {
        assert_eq!(engine_name(), "Spreadsheet Engine");
    }
    #[test]
    fn a1_addresses_round_trip_and_validate_limits() {
        for address in ["A1", "Z99", "AA1", "XFD1048576"] {
            let parsed = CellAddress::parse_a1(address).unwrap();
            assert_eq!(parsed.to_a1(), address);
        }
        assert!(CellAddress::parse_a1("A0").is_err());
        assert!(CellAddress::parse_a1("XFE1").is_err());
        assert!(CellAddress::parse_a1("1A").is_err());
    }
    #[test]
    fn worksheet_names_are_unique_and_last_sheet_cannot_be_removed() {
        let mut book = Workbook::new();
        assert!(book.add_sheet("sheet1").is_err());
        let second = book.add_sheet("Data").unwrap();
        assert!(book.remove_sheet(1).is_ok());
        assert!(book.remove_sheet(second).is_err());
    }
    #[test]
    fn formulas_evaluate_precedence_references_and_updates_without_stale_cache() {
        let mut book = Workbook::new();
        let sheet = book.sheets()[0].id();
        let a1 = CellAddress::parse_a1("A1").unwrap();
        let b1 = CellAddress::parse_a1("B1").unwrap();
        let c1 = CellAddress::parse_a1("C1").unwrap();
        book.set_number(sheet, a1, 10.0).unwrap();
        book.set_number(sheet, b1, 5.0).unwrap();
        book.set_formula(sheet, c1, "=A1+B1*2").unwrap();
        assert_eq!(book.value(sheet, c1), CellValue::Number(20.0));
        book.set_number(sheet, b1, 7.0).unwrap();
        assert_eq!(book.value(sheet, c1), CellValue::Number(24.0));
    }
    #[test]
    fn formulas_detect_cycles_and_division_by_zero() {
        let mut book = Workbook::new();
        let sheet = 1;
        let a1 = CellAddress::parse_a1("A1").unwrap();
        let b1 = CellAddress::parse_a1("B1").unwrap();
        book.set_formula(sheet, a1, "=B1+1").unwrap();
        book.set_formula(sheet, b1, "=A1+1").unwrap();
        assert_eq!(
            book.value(sheet, a1),
            CellValue::Error(FormulaError::CircularReference)
        );
        book.set_formula(sheet, b1, "=1/0").unwrap();
        assert_eq!(
            book.value(sheet, b1),
            CellValue::Error(FormulaError::DivisionByZero)
        );
    }
    #[test]
    fn formula_parser_rejects_bad_syntax_and_text_references() {
        let mut book = Workbook::new();
        let sheet = 1;
        let a1 = CellAddress::parse_a1("A1").unwrap();
        let b1 = CellAddress::parse_a1("B1").unwrap();
        book.set_text(sheet, a1, "hello").unwrap();
        book.set_formula(sheet, b1, "=A1+2").unwrap();
        assert_eq!(book.value(sheet, b1), CellValue::Error(FormulaError::Type));
        book.set_formula(sheet, b1, "=1+").unwrap();
        assert_eq!(book.value(sheet, b1), CellValue::Error(FormulaError::Parse));
    }
    #[test]
    fn workbook_round_trip_preserves_sheets_inputs_and_formulas() {
        let mut book = Workbook::new();
        let sheet = book.sheets()[0].id();
        let data = book.add_sheet("Data").unwrap();
        let a1 = CellAddress::parse_a1("A1").unwrap();
        let b1 = CellAddress::parse_a1("B1").unwrap();
        book.set_text(data, a1, "Quarter").unwrap();
        book.set_number(data, b1, 12.5).unwrap();
        book.set_formula(sheet, a1, "=B1*2").unwrap();
        let bytes = book.encode().unwrap();
        let restored = Workbook::decode(&bytes).unwrap();
        assert_eq!(restored, book);
        assert_eq!(restored.value(sheet, a1), CellValue::Number(0.0));
    }
    #[test]
    fn decoder_rejects_truncation_trailing_bytes_and_duplicate_cells() {
        let mut book = Workbook::new();
        book.set_number(1, CellAddress::parse_a1("A1").unwrap(), 1.0)
            .unwrap();
        let bytes = book.encode().unwrap();
        assert!(Workbook::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(Workbook::decode(&trailing).is_err());
    }
    #[test]
    fn invalid_edits_do_not_create_non_finite_numbers_or_bad_formulas() {
        let mut book = Workbook::new();
        assert!(
            book.set_number(1, CellAddress::parse_a1("A1").unwrap(), f64::NAN)
                .is_err()
        );
        assert!(
            book.set_formula(1, CellAddress::parse_a1("A1").unwrap(), "1+2")
                .is_err()
        );
        assert!(
            book.set_formula(1, CellAddress::parse_a1("A1").unwrap(), "=")
                .is_err()
        );
    }
}
