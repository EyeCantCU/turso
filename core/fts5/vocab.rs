//! The fts5vocab virtual table (fts5_vocab.c).

use super::sql::err;
use crate::native_ext::{ConnectInfo, VirtualTable, VirtualTableModule};
use crate::{Result, Value};

#[derive(Debug)]
pub(crate) struct VocabModule;

#[derive(Debug)]
pub(crate) struct VocabTable;

impl VirtualTableModule for VocabModule {
    type Table = super::Fts5Table;

    fn schema(&self, _args: &[Value]) -> Result<String> {
        Err(err("fts5vocab: not supported yet"))
    }

    fn create(&self, _args: &[Value]) -> Result<Self::Table> {
        Err(err("fts5vocab: not supported yet"))
    }

    fn connect(&self, _info: &ConnectInfo, _args: &[Value]) -> Result<(Self::Table, String)> {
        Err(err("fts5vocab: not supported yet"))
    }
}

#[allow(dead_code)]
fn _t(_: &dyn std::any::Any) -> Option<&dyn VirtualTable<Cursor = super::Fts5Cursor>> {
    None
}
