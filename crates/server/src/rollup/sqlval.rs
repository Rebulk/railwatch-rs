//! Reading telemetry columns the way Active Record casts them, for the jobs
//! that read rows back (rollups, grouping, detectors).

use rusqlite::types::ValueRef;

/// One column value, owned.
#[derive(Debug, Clone, PartialEq)]
pub enum Sql {
    Null,
    Int(i64),
    Real(f64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

impl Sql {
    pub fn from_ref(v: ValueRef<'_>) -> Sql {
        match v {
            ValueRef::Null => Sql::Null,
            ValueRef::Integer(i) => Sql::Int(i),
            ValueRef::Real(f) => Sql::Real(f),
            ValueRef::Text(t) => Sql::Text(t.to_vec()),
            ValueRef::Blob(b) => Sql::Blob(b.to_vec()),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Sql::Null)
    }

    /// `value.to_i` of an integer column (nil.to_i == 0).
    pub fn to_i(&self) -> i64 {
        match self {
            Sql::Null => 0,
            Sql::Int(i) => *i,
            Sql::Real(f) => *f as i64,
            Sql::Text(t) | Sql::Blob(t) => match crate::ingest::batch::string_to_i(t) {
                crate::rjson::Value::Int(i) => i,
                _ => 0,
            },
        }
    }

    /// The column as an Integer or nil.
    pub fn int(&self) -> Option<i64> {
        match self {
            Sql::Null => None,
            other => Some(other.to_i()),
        }
    }

    /// `"#{value}"` for a string column: nil interpolates as "".
    pub fn s(&self) -> &[u8] {
        match self {
            Sql::Text(t) | Sql::Blob(t) => t,
            _ => b"",
        }
    }

    pub fn text(&self) -> Option<&[u8]> {
        match self {
            Sql::Text(t) | Sql::Blob(t) => Some(t),
            _ => None,
        }
    }

    pub fn eq_str(&self, s: &str) -> bool {
        self.text() == Some(s.as_bytes())
    }

    /// Active Record's boolean cast (ActiveModel::Type::Boolean), truthiness
    /// of the result: nil and the FALSE_VALUES are false, "" casts to nil.
    pub fn truthy_bool(&self) -> bool {
        match self {
            Sql::Null => false,
            Sql::Int(i) => *i != 0,
            Sql::Real(_) => true,
            Sql::Text(t) | Sql::Blob(t) => !matches!(t.as_slice(), b"" | b"0" | b"f" | b"F" | b"false" | b"FALSE" | b"off" | b"OFF"),
        }
    }
}

pub fn row_values(row: &rusqlite::Row<'_>, n: usize) -> rusqlite::Result<Vec<Sql>> {
    (0..n).map(|i| row.get_ref(i).map(Sql::from_ref)).collect()
}
