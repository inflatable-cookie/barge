//! Shared test support for the CLI's contract: a fixture with its own state
//! directory and volume mount, a loopback `barged` stub, and the checker that
//! holds every JSON document to `docs/knowledge/contracts/consumption.md`.
//!
//! Each test target compiles this module for itself, so a helper only some
//! targets use is still fine: no target may inherit an unused-code warning.
//! The fixture helpers sit outside a `#[test]` function, where `clippy.toml`
//! grants the unwrap, expect and panic exception, so the module asks for it.
#![allow(dead_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use barge::testing::TempDir;
use serde_json::Value;

/// The identity the test-only seam hook reports for every fixture mount
/// (`BARGE_TEST_VOLUME_UUID`), so no test depends on the host's own volume.
pub const TEST_UUID: &str = "11111111-2222-3333-4444-555555555555";

pub struct Fixture {
    pub temp: TempDir,
    pub state: PathBuf,
    pub mount: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let temp = TempDir::new("cli").unwrap();
        let state = temp.folder("state").unwrap();
        let mount = temp.folder("Builds").unwrap();
        Self { temp, state, mount }
    }

    /// The environment every run gets: the fixture's own state directory and the
    /// fixed volume identity.
    pub fn barge(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_barge"));
        command
            .args(arguments)
            .env("BARGE_STATE_DIR", &self.state)
            .env("BARGE_TEST_VOLUME_UUID", TEST_UUID);
        command
    }

    pub fn run(&self, arguments: &[&str]) -> Output {
        self.barge(arguments)
            .output()
            .expect("run the barge binary")
    }

    pub fn json(&self, arguments: &[&str]) -> Output {
        let mut with_json = vec!["--json"];
        with_json.extend_from_slice(arguments);
        self.run(&with_json)
    }

    pub fn mount(&self) -> &str {
        self.mount.to_str().unwrap()
    }
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The parsed contract, read once per test process.
pub fn contract() -> &'static Contract {
    static CONTRACT: OnceLock<Contract> = OnceLock::new();
    CONTRACT.get_or_init(Contract::load)
}

/// Parse one success document and hold it to the documented contract.
pub fn parse_document(output: &Output) -> Value {
    let document: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}): {}{}",
            stdout(output),
            stderr(output)
        )
    });
    contract().check(&document);
    document
}

pub fn run_with_stdin(fixture: &Fixture, arguments: &[&str], input: &str) -> Output {
    let mut child = fixture
        .barge(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the barge binary");
    child
        .stdin
        .as_mut()
        .expect("a piped stdin")
        .write_all(input.as_bytes())
        .expect("write the answer");
    child.wait_with_output().expect("collect the run")
}

/// A canned `barged` on loopback: one answer per endpoint (first matching prefix
/// wins), up to four requests, each recorded so a test can prove whether
/// anything was sent at all.
pub type Routes = Vec<(&'static str, u16, &'static str)>;

pub fn stub_daemon(status: u16, body: &'static str) -> (u16, Arc<Mutex<Vec<String>>>) {
    stub_daemon_routes(vec![("/", status, body)])
}

pub fn stub_daemon_routes(routes: Routes) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    thread::spawn(move || {
        for stream in listener.incoming().take(4) {
            let Ok(mut stream) = stream else { break };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let read = stream.read(&mut buffer).unwrap_or(0);
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let Some((head, sent)) = text.split_once("\r\n\r\n") else {
                    continue;
                };
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if sent.len() >= length {
                    recorded.lock().unwrap().push(text);
                    break;
                }
            }
            let request = recorded.lock().unwrap().last().cloned().unwrap_or_default();
            let target = request.split_whitespace().nth(1).unwrap_or("/").to_string();
            let (status, body) = routes
                .iter()
                .find(|(path, _, _)| target.starts_with(path))
                .map(|(_, status, body)| (*status, *body))
                .unwrap_or((404, "{}"));
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (port, requests)
}

/// Register a machine whose daemon is the fixture's loopback stub.
pub fn register_machine(fixture: &Fixture, daemon_port: u16) {
    let output = fixture.run(&[
        "machine",
        "add",
        "--name",
        "pc",
        "--hostname",
        "127.0.0.1",
        "--mac",
        "00:11:22:33:44:ff",
        "--daemon-port",
        &daemon_port.to_string(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
}

// ---------------------------------------------------------------- the contract

/// One field of a document or shape: its name, whether it may be absent, and its
/// type expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub optional: bool,
    pub ty: Ty,
}

/// A type expression from `consumption.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    String,
    Int,
    Float,
    Bool,
    Null,
    /// `T?`: the value is null or a `T`.
    Optional(Box<Ty>),
    /// `[T]`.
    Array(Box<Ty>),
    /// `{name: T, name?: T, *: T}`. A `*` entry matches any key the named
    /// entries do not, which is how a map is written.
    Object(Vec<Field>),
    /// `A|B`.
    Union(Vec<Ty>),
    /// A shape from the Shapes table.
    Named(String),
}

impl Ty {
    fn render(&self) -> String {
        match self {
            Ty::String => "string".to_string(),
            Ty::Int => "int".to_string(),
            Ty::Float => "float".to_string(),
            Ty::Bool => "bool".to_string(),
            Ty::Null => "null".to_string(),
            Ty::Optional(inner) => format!("{}?", inner.render()),
            Ty::Array(inner) => format!("[{}]", inner.render()),
            Ty::Object(fields) => {
                let fields: Vec<String> = fields
                    .iter()
                    .map(|field| {
                        format!(
                            "{}{}: {}",
                            field.name,
                            if field.optional { "?" } else { "" },
                            field.ty.render()
                        )
                    })
                    .collect();
                format!("{{{}}}", fields.join(", "))
            }
            Ty::Union(items) => items.iter().map(Ty::render).collect::<Vec<_>>().join("|"),
            Ty::Named(name) => name.clone(),
        }
    }
}

/// The parsed contract: each document schema's `data` fields, and the shapes
/// those fields and their nested objects use.
#[derive(Debug)]
pub struct Contract {
    pub schemas: BTreeMap<String, Vec<Field>>,
    pub shapes: BTreeMap<String, Vec<Field>>,
    /// The schemas a checked document actually carried, so a documented document
    /// that no command produced is reported rather than passing by default.
    seen: Mutex<BTreeSet<String>>,
}

impl Contract {
    /// Read `consumption.md` from the repository this test target belongs to.
    pub fn load() -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/knowledge/contracts/consumption.md");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        let contract = Self::parse(&text);
        contract.check_one_shape_per_field();
        contract
    }

    /// Parse the two documented tables: `| Schema | ... |` and `| Shape | ... |`.
    pub fn parse(text: &str) -> Self {
        let mut schemas = BTreeMap::new();
        let mut shapes = BTreeMap::new();
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            let target = if line.trim_start().starts_with("| Schema |") {
                &mut schemas
            } else if line.trim_start().starts_with("| Shape |") {
                &mut shapes
            } else {
                continue;
            };
            // Skip the markdown separator row.
            if let Some(separator) = lines.peek() {
                if separator.starts_with("| ---") {
                    lines.next();
                }
            }
            for row in lines.by_ref() {
                if !row.trim_start().starts_with('|') {
                    break;
                }
                let cells: Vec<&str> = row.split('|').map(str::trim).collect();
                let (Some(name), Some(fields)) = (cells.get(1), cells.get(2)) else {
                    panic!("malformed contract row {row:?}");
                };
                let name = name.trim_matches('`').trim().to_string();
                target.insert(name, parse_fields(fields));
            }
        }
        assert!(
            !schemas.is_empty() && !shapes.is_empty(),
            "consumption.md documents no schemas or no shapes"
        );
        Self {
            schemas,
            shapes,
            seen: Mutex::new(BTreeSet::new()),
        }
    }

    /// The documented schemas no checked document has carried yet.
    pub fn unseen(&self) -> Vec<String> {
        let seen = self.seen.lock().unwrap();
        self.schemas
            .keys()
            .filter(|schema| !seen.contains(*schema))
            .cloned()
            .collect()
    }

    /// The brief's first contract rule: one field name never means two shapes in
    /// the success documents. The check is one-directional, so a rename that
    /// leaves an unused shape name behind is allowed.
    fn check_one_shape_per_field(&self) {
        let mut seen: BTreeMap<&str, (String, Ty)> = BTreeMap::new();
        for (schema, fields) in &self.schemas {
            for field in fields {
                let render = field.ty.render();
                match seen.get(field.name.as_str()) {
                    Some((other, ty)) if ty != &field.ty => panic!(
                        "field {:?} is {render} in {schema} but {} in {other}; give it one type",
                        field.name,
                        ty.render()
                    ),
                    Some(_) => {}
                    None => {
                        seen.insert(&field.name, (schema.clone(), field.ty.clone()));
                    }
                }
            }
        }
    }

    /// Hold one success document to the contract: a known schema, exactly the
    /// documented `data` fields, and every field's documented type.
    pub fn check(&self, document: &Value) {
        let object = document
            .as_object()
            .unwrap_or_else(|| panic!("a document is not an object: {document}"));
        assert_eq!(
            object.len(),
            2,
            "a success document has exactly `schema` and `data`: {document}"
        );
        let schema = object["schema"]
            .as_str()
            .unwrap_or_else(|| panic!("a document has no schema string: {document}"));
        let fields = self
            .schemas
            .get(schema)
            .unwrap_or_else(|| panic!("{schema} is not documented in consumption.md"));
        self.seen.lock().unwrap().insert(schema.to_string());
        self.check_fields(fields, &object["data"], schema);
    }

    /// Hold a standalone document to one documented shape. A move plan is
    /// printed on its own (`barge move --plan`), not inside an envelope.
    pub fn check_shape(&self, name: &str, value: &Value) {
        let fields = self
            .shapes
            .get(name)
            .unwrap_or_else(|| panic!("shape {name} is not documented"));
        self.check_fields(fields, value, name);
    }

    /// Hold an error document to the documented envelope.
    pub fn check_error(&self, document: &Value) {
        let object = document
            .as_object()
            .unwrap_or_else(|| panic!("an error document is not an object: {document}"));
        assert_eq!(
            object.keys().collect::<Vec<_>>(),
            vec!["error", "schema"],
            "an error document has exactly `schema` and `error`: {document}"
        );
        assert_eq!(object["schema"], "barge.error/1");
        let error = object["error"].as_object().expect("an error object");
        assert_eq!(error.len(), 2, "error holds kind and message: {document}");
        assert!(error["kind"].is_string(), "{document}");
        assert!(error["message"].is_string(), "{document}");
    }

    fn check_fields(&self, fields: &[Field], value: &Value, path: &str) {
        if let Err(reason) = self.validate_fields(fields, value, path, &mut Vec::new()) {
            panic!("{reason}");
        }
    }

    fn validate_fields(
        &self,
        fields: &[Field],
        value: &Value,
        path: &str,
        seen: &mut Vec<String>,
    ) -> Result<(), String> {
        let Some(object) = value.as_object() else {
            return Err(format!("{path} is not an object: {value}"));
        };
        let wildcard = fields.iter().find(|field| field.name == "*");
        for (name, value) in object {
            match fields.iter().find(|field| &field.name == name) {
                Some(field) => self.validate(&field.ty, value, &format!("{path}.{name}"), seen)?,
                None => match wildcard {
                    Some(field) => {
                        self.validate(&field.ty, value, &format!("{path}.{name}"), seen)?
                    }
                    None => {
                        return Err(format!(
                            "{path} has an undocumented field {name:?}: {value}"
                        ))
                    }
                },
            }
        }
        for field in fields {
            if !field.optional && field.name != "*" && object.get(&field.name).is_none() {
                return Err(format!(
                    "{path} is missing the documented field {:?}",
                    field.name
                ));
            }
        }
        Ok(())
    }

    fn validate(
        &self,
        ty: &Ty,
        value: &Value,
        path: &str,
        seen: &mut Vec<String>,
    ) -> Result<(), String> {
        match ty {
            Ty::String if !value.is_string() => Err(format!("{path} is not a string: {value}")),
            Ty::Int if value.as_u64().is_none() && value.as_i64().is_none() => {
                Err(format!("{path} is not an integer: {value}"))
            }
            Ty::Float if !value.is_f64() => Err(format!("{path} is not a float: {value}")),
            Ty::Bool if !value.is_boolean() => Err(format!("{path} is not a bool: {value}")),
            Ty::Null if !value.is_null() => Err(format!("{path} is not null: {value}")),
            Ty::String | Ty::Int | Ty::Float | Ty::Bool | Ty::Null => Ok(()),
            Ty::Optional(inner) => {
                if value.is_null() {
                    Ok(())
                } else {
                    self.validate(inner, value, path, seen)
                }
            }
            Ty::Array(inner) => {
                let Some(items) = value.as_array() else {
                    return Err(format!("{path} is not an array: {value}"));
                };
                for (index, item) in items.iter().enumerate() {
                    self.validate(inner, item, &format!("{path}[{index}]"), seen)?;
                }
                Ok(())
            }
            Ty::Object(fields) => self.validate_fields(fields, value, path, seen),
            Ty::Union(items) => {
                let mut reasons = Vec::new();
                for item in items {
                    match self.validate(item, value, path, &mut seen.clone()) {
                        Ok(()) => return Ok(()),
                        Err(reason) => reasons.push(reason),
                    }
                }
                Err(format!(
                    "{path} matches none of {}: {reasons:?}",
                    ty.render()
                ))
            }
            Ty::Named(name) => {
                if seen.contains(name) {
                    return Err(format!("{path}: shape {name} refers to itself"));
                }
                let Some(fields) = self.shapes.get(name) else {
                    return Err(format!("{path}: shape {name} is not documented"));
                };
                seen.push(name.clone());
                self.validate_fields(fields, value, path, seen)?;
                seen.pop();
                Ok(())
            }
        }
    }
}

fn parse_fields(cell: &str) -> Vec<Field> {
    let text = cell.replace('`', "");
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    split_top_level(text, ',')
        .into_iter()
        .map(|entry| {
            let (name, ty) = entry
                .split_once(':')
                .unwrap_or_else(|| panic!("malformed field {entry:?}"));
            let mut name = name.trim().to_string();
            let optional = name.ends_with('?');
            if optional {
                name.pop();
            }
            Field {
                name,
                optional,
                ty: parse_type(&mut Lexer::new(ty)),
            }
        })
        .collect()
}

/// Split on `separator` outside brackets and braces.
fn split_top_level(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0_i32;
    let mut start = 0;
    for (index, character) in text.char_indices() {
        match character {
            '[' | '{' => depth += 1,
            ']' | '}' => depth -= 1,
            character if character == separator && depth == 0 => {
                parts.push(text[start..index].trim());
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(text[start..].trim());
    parts
}

struct Lexer<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, at: 0 }
    }

    fn skip_space(&mut self) {
        while let Some(character) = self.text[self.at..].chars().next() {
            if character.is_whitespace() {
                self.at += character.len_utf8();
            } else {
                break;
            }
        }
    }

    fn peek(&mut self) -> Option<char> {
        self.skip_space();
        self.text[self.at..].chars().next()
    }

    fn take(&mut self, expected: char) {
        assert_eq!(
            self.peek(),
            Some(expected),
            "expected {expected:?} in {:?}",
            self.text
        );
        self.at += expected.len_utf8();
    }

    fn identifier(&mut self) -> String {
        self.skip_space();
        let start = self.at;
        while let Some(character) = self.text[self.at..].chars().next() {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                self.at += character.len_utf8();
            } else {
                break;
            }
        }
        assert!(self.at > start, "expected a type in {:?}", self.text);
        self.text[start..self.at].to_string()
    }
}

fn parse_type(lexer: &mut Lexer<'_>) -> Ty {
    let mut items = vec![parse_atom(lexer)];
    while lexer.peek() == Some('|') {
        lexer.take('|');
        items.push(parse_atom(lexer));
    }
    if items.len() == 1 {
        items.pop().unwrap()
    } else {
        Ty::Union(items)
    }
}

fn parse_atom(lexer: &mut Lexer<'_>) -> Ty {
    let mut ty = match lexer.peek() {
        Some('[') => {
            lexer.take('[');
            let inner = parse_type(lexer);
            lexer.take(']');
            Ty::Array(Box::new(inner))
        }
        Some('{') => {
            lexer.take('{');
            let mut fields = Vec::new();
            while lexer.peek() != Some('}') {
                let name = if lexer.peek() == Some('*') {
                    lexer.take('*');
                    "*".to_string()
                } else {
                    lexer.identifier()
                };
                let optional = lexer.peek() == Some('?');
                if optional {
                    lexer.take('?');
                }
                lexer.take(':');
                let ty = parse_type(lexer);
                fields.push(Field { name, optional, ty });
                if lexer.peek() == Some(',') {
                    lexer.take(',');
                } else {
                    break;
                }
            }
            lexer.take('}');
            Ty::Object(fields)
        }
        Some(_) => match lexer.identifier().as_str() {
            "string" => Ty::String,
            "int" => Ty::Int,
            "float" => Ty::Float,
            "bool" => Ty::Bool,
            "null" => Ty::Null,
            name => Ty::Named(name.to_string()),
        },
        None => panic!("a type expression ended early"),
    };
    if lexer.peek() == Some('?') {
        lexer.take('?');
        ty = Ty::Optional(Box::new(ty));
    }
    ty
}

#[cfg(test)]
mod tests {
    use super::{parse_fields, Contract, Ty};

    #[test]
    fn the_documented_contract_parses_and_names_one_shape_per_field() {
        let contract = Contract::load();
        assert!(contract.schemas.contains_key("barge.status/1"));
        assert!(contract.shapes.contains_key("volume-status"));
    }

    #[test]
    fn type_expressions_parse_every_documented_form() {
        let fields = parse_fields("`a: string`, `b?: [int]`, `c: {x: bool, y?: null}`");
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].ty, Ty::String);
        assert!(fields[1].optional);
        assert_eq!(fields[1].ty, Ty::Array(Box::new(Ty::Int)));
        assert!(matches!(fields[2].ty, Ty::Object(_)));
    }
}
