//! BM25 search with tantivy.
//!
//! The index lives in memory and is rebuilt from the database at start, so it
//! never needs a migration or a backup.

use crate::BrainError;
use std::sync::Mutex;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, STORED, STRING, Schema, TEXT, Value};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term, doc};
use uuid::Uuid;

const WRITER_MEMORY: usize = 15_000_000;

fn failed(error: impl std::fmt::Display) -> BrainError {
    BrainError::Index(error.to_string())
}

/// The BM25 index over chunks.
pub struct LexicalIndex {
    index: Index,
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    entry: Field,
    text: Field,
}

impl std::fmt::Debug for LexicalIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LexicalIndex").finish_non_exhaustive()
    }
}

impl LexicalIndex {
    /// Creates an empty index.
    pub fn new() -> Result<Self, BrainError> {
        let mut builder = Schema::builder();
        let entry = builder.add_text_field("entry", STRING | STORED);
        let text = builder.add_text_field("text", TEXT);
        let index = Index::create_in_ram(builder.build());
        let writer = index.writer(WRITER_MEMORY).map_err(failed)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(failed)?;
        Ok(Self {
            index,
            reader,
            writer: Mutex::new(writer),
            entry,
            text,
        })
    }

    /// Replaces the chunks of an entry.
    pub fn replace(&self, entry: Uuid, chunks: &[String]) -> Result<(), BrainError> {
        let id = entry.to_string();
        let mut writer = self.writer.lock().expect("index writer lock");
        writer.delete_term(Term::from_field_text(self.entry, &id));
        for chunk in chunks {
            writer
                .add_document(doc!(self.entry => id.as_str(), self.text => chunk.as_str()))
                .map_err(failed)?;
        }
        writer.commit().map_err(failed)?;
        self.reader.reload().map_err(failed)
    }

    /// Returns entries by rank, best first, each at its best chunk.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Uuid>, BrainError> {
        let parser = QueryParser::for_index(&self.index, vec![self.text]);
        let (query, _) = parser.parse_query_lenient(query);
        let searcher = self.reader.searcher();
        let hits = searcher
            .search(&query, &TopDocs::with_limit(limit).order_by_score())
            .map_err(failed)?;
        let mut ranked = Vec::new();
        for (_, address) in hits {
            let document: TantivyDocument = searcher.doc(address).map_err(failed)?;
            let id = document
                .get_first(self.entry)
                .and_then(|value| value.as_str())
                .and_then(|value| Uuid::parse_str(value).ok());
            if let Some(id) = id
                && !ranked.contains(&id)
            {
                ranked.push(id);
            }
        }
        Ok(ranked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_ranks_and_forgets_entries() {
        let index = LexicalIndex::new().unwrap();
        let (first, second) = (Uuid::now_v7(), Uuid::now_v7());
        index
            .replace(first, &["tax logic for invoices".to_owned()])
            .unwrap();
        index
            .replace(second, &["tax tax tax report".to_owned()])
            .unwrap();
        assert_eq!(index.search("tax", 10).unwrap(), [second, first]);
        assert_eq!(index.search("invoices", 10).unwrap(), [first]);
        index.replace(first, &[]).unwrap();
        assert!(index.search("invoices", 10).unwrap().is_empty());
    }

    #[test]
    fn query_syntax_errors_do_not_fail() {
        let index = LexicalIndex::new().unwrap();
        index
            .replace(Uuid::now_v7(), &["plain text".to_owned()])
            .unwrap();
        assert_eq!(index.search("text AND (", 10).unwrap().len(), 1);
    }
}
