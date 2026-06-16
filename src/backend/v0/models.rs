use glob::Pattern;
use rusqlite::{
    ToSql,
    types::{FromSql, FromSqlResult, ToSqlOutput, ValueRef},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Deserialize, Serialize, Debug)]
pub struct Code {
    pub code: String,
}

#[derive(Deserialize, Serialize, Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum ProgrammingLanguage {
    #[serde(rename = "rust")]       Rust,
    #[serde(rename = "python")]     Python,
    #[serde(rename = "javascript")] JavaScript,
    #[serde(rename = "typescript")] TypeScript,
    #[serde(rename = "tsx")]        Tsx,
    #[serde(rename = "go")]         Go,
    #[serde(rename = "c")]          C,
    #[serde(rename = "cpp")]        Cpp,
    #[serde(rename = "java")]       Java,
    #[serde(rename = "csharp")]     CSharp,
    #[serde(rename = "ruby")]       Ruby,
    #[serde(rename = "php")]        Php,
    #[serde(rename = "bash")]       Bash,
    #[serde(rename = "html")]       Html,
    #[serde(rename = "css")]        Css,
    #[serde(rename = "json")]       Json,
    #[serde(rename = "scala")]      Scala,
    #[serde(rename = "haskell")]    Haskell,
    #[serde(rename = "ocaml")]      Ocaml,
    #[serde(rename = "zig")]        Zig,
    #[serde(rename = "sql")]        Sql,
}

impl ToSql for ProgrammingLanguage {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        let val = match self {
            ProgrammingLanguage::Rust       => "rust",
            ProgrammingLanguage::Python     => "python",
            ProgrammingLanguage::JavaScript => "javascript",
            ProgrammingLanguage::TypeScript => "typescript",
            ProgrammingLanguage::Tsx        => "tsx",
            ProgrammingLanguage::Go         => "go",
            ProgrammingLanguage::C          => "c",
            ProgrammingLanguage::Cpp        => "cpp",
            ProgrammingLanguage::Java       => "java",
            ProgrammingLanguage::CSharp     => "csharp",
            ProgrammingLanguage::Ruby       => "ruby",
            ProgrammingLanguage::Php        => "php",
            ProgrammingLanguage::Bash       => "bash",
            ProgrammingLanguage::Html       => "html",
            ProgrammingLanguage::Css        => "css",
            ProgrammingLanguage::Json       => "json",
            ProgrammingLanguage::Scala      => "scala",
            ProgrammingLanguage::Haskell    => "haskell",
            ProgrammingLanguage::Ocaml      => "ocaml",
            ProgrammingLanguage::Zig        => "zig",
            ProgrammingLanguage::Sql        => "sql",
        };
        Ok(ToSqlOutput::from(val))
    }
}

impl FromSql for ProgrammingLanguage {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "rust"       => Ok(ProgrammingLanguage::Rust),
            "python"     => Ok(ProgrammingLanguage::Python),
            "javascript" => Ok(ProgrammingLanguage::JavaScript),
            "typescript" => Ok(ProgrammingLanguage::TypeScript),
            "tsx"        => Ok(ProgrammingLanguage::Tsx),
            "go"         => Ok(ProgrammingLanguage::Go),
            "c"          => Ok(ProgrammingLanguage::C),
            "cpp"        => Ok(ProgrammingLanguage::Cpp),
            "java"       => Ok(ProgrammingLanguage::Java),
            "csharp"     => Ok(ProgrammingLanguage::CSharp),
            "ruby"       => Ok(ProgrammingLanguage::Ruby),
            "php"        => Ok(ProgrammingLanguage::Php),
            "bash"       => Ok(ProgrammingLanguage::Bash),
            "html"       => Ok(ProgrammingLanguage::Html),
            "css"        => Ok(ProgrammingLanguage::Css),
            "json"       => Ok(ProgrammingLanguage::Json),
            "scala"      => Ok(ProgrammingLanguage::Scala),
            "haskell"    => Ok(ProgrammingLanguage::Haskell),
            "ocaml"      => Ok(ProgrammingLanguage::Ocaml),
            "zig"        => Ok(ProgrammingLanguage::Zig),
            "sql"        => Ok(ProgrammingLanguage::Sql),
            _            => Err(rusqlite::types::FromSqlError::InvalidType),
        }
    }
}

type UnixPath = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct UUIDv4(pub Uuid);

impl ToSql for UUIDv4 {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0.simple().to_string()))
    }
}

impl FromSql for UUIDv4 {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        let uuid = Uuid::parse_str(s).map_err(|_| rusqlite::types::FromSqlError::InvalidType)?;
        Ok(UUIDv4(uuid))
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub struct IndexRequest {
    pub files: HashMap<ProgrammingLanguage, HashMap<UnixPath, Code>>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct IndexResponse {
    pub files: HashMap<ProgrammingLanguage, HashMap<UnixPath, u64>>,
}

#[derive(Debug)]
pub struct GlobPattern(pub Pattern);

impl<'l> Deserialize<'l> for GlobPattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'l>,
    {
        let s = String::deserialize(deserializer)?;
        Pattern::new(&s)
            .map(GlobPattern)
            .map_err(serde::de::Error::custom)
    }
}

impl Serialize for GlobPattern {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.as_str().serialize(serializer)
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub struct SearchFilter {
    pub paths: Option<Vec<GlobPattern>>,
    pub programming_languages: Option<Vec<ProgrammingLanguage>>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct SearchRequest {
    pub query: String,
    pub top_k: Option<usize>,
    pub include: Option<SearchFilter>,
    pub exclude: Option<SearchFilter>,
}

#[derive(Serialize, Debug)]
pub struct SearchResult {
    pub score: f32,
    pub path: UnixPath,
    pub code: String,
    pub start_line: usize,
    pub end_line: usize,
    pub start_column: usize,
    pub end_column: usize,
}

#[derive(Serialize, Debug)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
}
