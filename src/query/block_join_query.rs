// src/query/block_join_query.rs

use crate::core::searcher::Searcher;
use crate::query::{EnableScoring, Explanation, Query, QueryClone, Scorer, Weight};
use crate::schema::Term;
use crate::{DocAddress, DocId, DocSet, Result, Score, SegmentReader, TERMINATED};
use common::BitSet;
use std::fmt;

/// How scores should be aggregated from child documents.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScoreMode {
    /// Use the average of all child scores as the parent score.
    Avg,
    /// Use the maximum child score as the parent score.
    Max,
    /// Sum all child scores for the parent score.
    Total,
    /// Do not score parent docs from child docs. Just rely on parent scoring.
    None,
}

impl Default for ScoreMode {
    fn default() -> Self {
        ScoreMode::Avg
    }
}

/// `BlockJoinQuery` performs a join from child documents to parent documents,
/// based on a block structure: child documents are indexed before their parent.
/// The `parents_filter` identifies the parent documents in each segment.
pub struct BlockJoinQuery {
    child_query: Box<dyn Query>,
    parents_filter: Box<dyn Query>,
    score_mode: ScoreMode,
}

impl Clone for BlockJoinQuery {
    fn clone(&self) -> Self {
        BlockJoinQuery {
            child_query: self.child_query.box_clone(),
            parents_filter: self.parents_filter.box_clone(),
            score_mode: self.score_mode,
        }
    }
}

impl BlockJoinQuery {
    /// Creates a new `BlockJoinQuery`.
    ///
    /// # Arguments
    ///
    /// * `child_query` - The query to match child documents.
    /// * `parents_filter` - The query to identify parent documents.
    /// * `score_mode` - The mode to aggregate scores from child documents.
    pub fn new(
        child_query: Box<dyn Query>,
        parents_filter: Box<dyn Query>,
        score_mode: ScoreMode,
    ) -> BlockJoinQuery {
        BlockJoinQuery {
            child_query,
            parents_filter,
            score_mode,
        }
    }
}

impl fmt::Debug for BlockJoinQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "BlockJoinQuery(child_query: {:?}, parents_filter: {:?}, score_mode: {:?})",
            self.child_query, self.parents_filter, self.score_mode
        )
    }
}

impl Query for BlockJoinQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> Result<Box<dyn Weight>> {
        println!("BlockJoinQuery::weight() - Creating weights");
        let child_weight = self.child_query.weight(enable_scoring.clone())?;
        println!("BlockJoinQuery::weight() - Created child weight");
        let parents_weight = self.parents_filter.weight(enable_scoring)?;
        println!("BlockJoinQuery::weight() - Created parent weight");

        Ok(Box::new(BlockJoinWeight {
            child_weight,
            parents_weight,
            score_mode: self.score_mode,
        }))
    }

    fn explain(&self, searcher: &Searcher, doc_address: DocAddress) -> Result<Explanation> {
        let reader = searcher.segment_reader(doc_address.segment_ord);
        let mut scorer = self
            .weight(EnableScoring::enabled_from_searcher(searcher))?
            .scorer(reader, 1.0)?;

        let mut current_doc = scorer.doc();
        while current_doc != TERMINATED && current_doc < doc_address.doc_id {
            current_doc = scorer.advance();
        }

        let score = if current_doc == doc_address.doc_id {
            scorer.score()
        } else {
            0.0
        };

        let mut explanation = Explanation::new("BlockJoinQuery", score);
        explanation.add_detail(Explanation::new("score", score));
        Ok(explanation)
    }

    fn count(&self, searcher: &Searcher) -> Result<usize> {
        let weight = self.weight(EnableScoring::disabled_from_searcher(searcher))?;
        let mut total_count = 0;
        for reader in searcher.segment_readers() {
            total_count += weight.count(reader)? as usize;
        }
        Ok(total_count)
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {
        self.child_query.query_terms(visitor);
        self.parents_filter.query_terms(visitor);
    }
}

struct BlockJoinWeight {
    child_weight: Box<dyn Weight>,
    parents_weight: Box<dyn Weight>,
    score_mode: ScoreMode,
}

impl Weight for BlockJoinWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> Result<Box<dyn Scorer>> {
        println!(
            "BlockJoinWeight::scorer() - Creating scorer with boost {}",
            boost
        );

        let max_doc = reader.max_doc();
        println!("BlockJoinWeight::scorer() - Max doc value: {}", max_doc);
        let mut parents_bitset = BitSet::with_max_value(max_doc);

        // Get parent documents first
        println!("BlockJoinWeight::scorer() - Creating parent scorer");
        let mut parents_scorer = self.parents_weight.scorer(reader, boost)?;
        println!("BlockJoinWeight::scorer() - Parent scorer created");

        let mut found_parent = false;
        let mut parent_count = 0;
        while parents_scorer.doc() != TERMINATED {
            let parent_doc = parents_scorer.doc();
            println!(
                "BlockJoinWeight::scorer() - Found parent doc: {}",
                parent_doc
            );
            parents_bitset.insert(parent_doc);
            found_parent = true;
            parent_count += 1;
            parents_scorer.advance();
        }
        println!(
            "BlockJoinWeight::scorer() - Found {} parent documents",
            parent_count
        );

        if !found_parent {
            println!("BlockJoinWeight::scorer() - No parents found, returning empty scorer");
            return Ok(Box::new(EmptyScorer));
        }

        println!("BlockJoinWeight::scorer() - Creating child scorer");
        let child_scorer = self.child_weight.scorer(reader, boost)?;
        println!("BlockJoinWeight::scorer() - Child scorer created");

        // Initialize with first parent
        let mut first_parent = TERMINATED;
        for i in 0..=max_doc {
            if parents_bitset.contains(i) {
                first_parent = i;
                break;
            }
        }

        println!(
            "BlockJoinWeight::scorer() - Creating BlockJoinScorer (first_parent: {})",
            first_parent
        );
        let mut scorer = BlockJoinScorer {
            child_scorer,
            parent_docs: parents_bitset,
            score_mode: self.score_mode,
            current_parent: first_parent,
            previous_parent: None,
            current_score: 1.0,
            initialized: false,
            has_more: first_parent != TERMINATED,
        };
        Ok(Box::new(scorer))
    }

    fn explain(&self, _reader: &SegmentReader, _doc: DocId) -> Result<Explanation> {
        unimplemented!("Explain is not implemented for BlockJoinWeight");
    }

    fn count(&self, reader: &SegmentReader) -> Result<u32> {
        let mut count = 0;
        let mut scorer = self.scorer(reader, 1.0)?;
        while scorer.doc() != TERMINATED {
            count += 1;
            scorer.advance();
        }
        Ok(count)
    }
}

struct EmptyScorer;

impl DocSet for EmptyScorer {
    fn advance(&mut self) -> DocId {
        TERMINATED
    }

    fn doc(&self) -> DocId {
        TERMINATED
    }

    fn size_hint(&self) -> u32 {
        0
    }
}

impl Scorer for EmptyScorer {
    fn score(&mut self) -> Score {
        0.0
    }
}

struct BlockJoinScorer {
    child_scorer: Box<dyn Scorer>,
    parent_docs: BitSet,
    score_mode: ScoreMode,
    current_parent: DocId,
    previous_parent: Option<DocId>,
    current_score: Score,
    initialized: bool,
    has_more: bool,
}

impl DocSet for BlockJoinScorer {
    fn advance(&mut self) -> DocId {
        if !self.has_more {
            return TERMINATED;
        }

        if !self.initialized {
            self.initialized = true;
            self.previous_parent = None;
            self.collect_matches();
            return self.current_parent;
        }

        // Find next parent after current one
        let next_parent = self.find_next_parent(self.current_parent + 1);
        if next_parent == TERMINATED {
            self.has_more = false;
            self.current_parent = TERMINATED;
            return TERMINATED;
        }

        self.previous_parent = Some(self.current_parent);
        self.current_parent = next_parent;
        self.collect_matches();
        self.current_parent
    }

    fn doc(&self) -> DocId {
        if !self.initialized {
            TERMINATED
        } else if self.has_more {
            self.current_parent
        } else {
            TERMINATED
        }
    }

    fn size_hint(&self) -> u32 {
        self.parent_docs.len() as u32
    }
}

impl BlockJoinScorer {
    fn initialize(&mut self) {
        println!("Initializing BlockJoinScorer...");
        if !self.initialized {
            // Initialize the child scorer
            let _child_doc = self.child_scorer.advance();

            // Find the first parent
            let first_parent = self.find_next_parent(0);
            if first_parent != TERMINATED {
                self.current_parent = first_parent;
                self.has_more = true;
                self.collect_matches();
            } else {
                self.has_more = false;
                self.current_parent = TERMINATED;
            }

            self.initialized = true;
            println!(
                "Initialization complete: current_parent={}, has_more={}",
                self.current_parent, self.has_more
            );
        }
    }

    fn find_next_parent(&self, from: DocId) -> DocId {
        println!("Finding next parent from {}", from);
        let mut current = from;
        let max_val = self.parent_docs.max_value();

        while current <= max_val {
            if self.parent_docs.contains(current) {
                println!("Found parent at {}", current);
                return current;
            }
            current += 1;
        }
        println!("No more parents found");
        TERMINATED
    }

    fn collect_matches(&mut self) {
        let mut child_scores = Vec::new();

        // Determine the starting document ID for collecting child documents
        let start_doc = match self.previous_parent {
            Some(prev_parent_doc) => prev_parent_doc + 1,
            None => 0,
        };

        // Advance the child_scorer to the start_doc if necessary
        let mut current_child = self.child_scorer.doc();
        while current_child != TERMINATED && current_child < start_doc {
            current_child = self.child_scorer.advance();
        }

        let end_doc = self.current_parent;

        // Collect all child documents between start_doc and end_doc
        while current_child != TERMINATED && current_child < end_doc {
            child_scores.push(self.child_scorer.score());
            current_child = self.child_scorer.advance();
        }

        // Aggregate the scores according to the score_mode
        self.current_score = match self.score_mode {
            ScoreMode::Avg => {
                if child_scores.is_empty() {
                    1.0
                } else {
                    child_scores.iter().sum::<Score>() / child_scores.len() as Score
                }
            }
            ScoreMode::Max => {
                if child_scores.is_empty() {
                    1.0
                } else {
                    child_scores.iter().cloned().fold(f32::MIN, f32::max)
                }
            }
            ScoreMode::Total => {
                if child_scores.is_empty() {
                    1.0
                } else {
                    child_scores.iter().sum()
                }
            }
            ScoreMode::None => 1.0,
        };
    }
}

impl Scorer for BlockJoinScorer {
    fn score(&mut self) -> Score {
        self.current_score
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::TopDocs;
    use crate::query::TermQuery;
    use crate::schema::{Field, IndexRecordOption, Schema, Value, STORED, STRING};
    use crate::{DocAddress, Index, IndexWriter, TantivyDocument, Term};

    /// Helper function to create a test index with parent and child documents.
    fn create_test_index() -> crate::Result<(Index, Field, Field, Field, Field)> {
        let mut schema_builder = Schema::builder();
        let name_field = schema_builder.add_text_field("name", STRING | STORED);
        let country_field = schema_builder.add_text_field("country", STRING | STORED);
        let skill_field = schema_builder.add_text_field("skill", STRING | STORED);
        let doc_type_field = schema_builder.add_text_field("docType", STRING);
        let schema = schema_builder.build();

        let index = Index::create_in_ram(schema);
        {
            let mut index_writer: IndexWriter = index.writer_for_tests()?;

            // First block:
            // children docs first, parent doc last
            index_writer.add_documents(vec![
                doc!(
                    skill_field => "java",
                    doc_type_field => "job"
                ),
                doc!(
                    skill_field => "python",
                    doc_type_field => "job"
                ),
                doc!(
                    skill_field => "java",
                    doc_type_field => "job"
                ),
                // parent last in this block
                doc!(
                    name_field => "Lisa",
                    country_field => "United Kingdom",
                    doc_type_field => "resume" // Consistent identifier for parent
                ),
            ])?;

            // Second block:
            index_writer.add_documents(vec![
                doc!(
                    skill_field => "ruby",
                    doc_type_field => "job"
                ),
                doc!(
                    skill_field => "java",
                    doc_type_field => "job"
                ),
                // parent last in this block
                doc!(
                    name_field => "Frank",
                    country_field => "United States",
                    doc_type_field => "resume" // Consistent identifier for parent
                ),
            ])?;

            index_writer.commit()?;
        }
        Ok((
            index,
            name_field,
            country_field,
            skill_field,
            doc_type_field,
        ))
    }

    #[test]
    pub fn test_simple_block_join() -> crate::Result<()> {
        let (index, name_field, _country_field, skill_field, doc_type_field) = create_test_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"), // Updated from "parent" to "resume"
            IndexRecordOption::Basic,
        );

        let child_query = TermQuery::new(
            Term::from_field_text(skill_field, "java"),
            IndexRecordOption::Basic,
        );

        let block_join_query = BlockJoinQuery::new(
            Box::new(child_query),
            Box::new(parent_query),
            ScoreMode::Avg,
        );

        let top_docs = searcher.search(&block_join_query, &TopDocs::with_limit(1))?;
        assert_eq!(top_docs.len(), 1, "Should find 1 top document");

        let doc: TantivyDocument = searcher.doc(top_docs[0].1)?;
        assert_eq!(
            doc.get_first(name_field).unwrap().as_str().unwrap(),
            "Lisa",
            "Expected top document to be 'Lisa'"
        );

        Ok(())
    }

    #[test]
    pub fn test_block_join_no_matches() -> crate::Result<()> {
        let (index, name_field, country_field, skill_field, doc_type_field) = create_test_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"), // Updated from "parent" to "resume"
            IndexRecordOption::Basic,
        );

        let child_query = TermQuery::new(
            Term::from_field_text(skill_field, "java"),
            IndexRecordOption::Basic,
        );

        let block_join_query = BlockJoinQuery::new(
            Box::new(child_query),
            Box::new(parent_query),
            ScoreMode::Avg,
        );

        let top_docs = searcher.search(&block_join_query, &TopDocs::with_limit(1))?;
        assert_eq!(top_docs.len(), 1, "Should find 1 top document");

        let doc: TantivyDocument = searcher.doc(top_docs[0].1)?;
        assert_eq!(
            doc.get_first(name_field).unwrap().as_str().unwrap(),
            "Frank",
            "Expected top document to be 'Frank'"
        );

        Ok(())
    }

    #[test]
    pub fn test_block_join_scoring() -> crate::Result<()> {
        let (index, _name_field, _country_field, skill_field, doc_type_field) =
            create_test_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"), // Updated from "parent" to "resume"
            IndexRecordOption::WithFreqs,
        );

        let child_query = TermQuery::new(
            Term::from_field_text(skill_field, "java"),
            IndexRecordOption::WithFreqs,
        );

        let block_join_query = BlockJoinQuery::new(
            Box::new(child_query),
            Box::new(parent_query),
            ScoreMode::Avg,
        );

        let top_docs = searcher.search(&block_join_query, &TopDocs::with_limit(1))?;
        assert_eq!(top_docs.len(), 1, "Should find 1 top document");

        // Score should be influenced by children, ensure it's not zero
        assert!(
            top_docs[0].0 > 0.0,
            "Top document score should be greater than 0.0"
        );

        Ok(())
    }

    #[test]
    pub fn test_explain_block_join() -> crate::Result<()> {
        let (index, _name_field, country_field, skill_field, doc_type_field) = create_test_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"), // Updated from "parent" to "resume"
            IndexRecordOption::Basic,
        );

        let child_query = TermQuery::new(
            Term::from_field_text(skill_field, "java"),
            IndexRecordOption::Basic,
        );

        let block_join_query = BlockJoinQuery::new(
            Box::new(child_query),
            Box::new(parent_query),
            ScoreMode::Avg,
        );

        // The parent doc for "United Kingdom" is doc 3 in the first segment
        let explanation = block_join_query.explain(&searcher, DocAddress::new(0, 3))?;
        assert!(
            explanation.value() > 0.0,
            "Explanation score should be greater than 0.0"
        );

        Ok(())
    }
}

// src/query/block_join_query.rs

#[cfg(test)]
mod atomic_tests {
    use super::*;
    use crate::collector::TopDocs;
    use crate::query::TermQuery;
    use crate::schema::{Field, IndexRecordOption, Schema, Value, STORED, STRING};
    use crate::{DocAddress, Index, IndexWriter, TantivyDocument, Term};

    /// Helper function to create a very simple test index with just one parent and one child
    fn create_minimal_index() -> crate::Result<(Index, Field, Field)> {
        let mut schema_builder = Schema::builder();
        let content_field = schema_builder.add_text_field("content", STRING | STORED);
        let doc_type_field = schema_builder.add_text_field("docType", STRING);
        let schema = schema_builder.build();

        let index = Index::create_in_ram(schema);
        {
            let mut index_writer: IndexWriter = index.writer_for_tests()?;

            // Add one child and one parent
            index_writer.add_documents(vec![
                doc!(
                    content_field => "child content",
                    doc_type_field => "child"
                ),
                doc!(
                    content_field => "resume content", // Changed from "parent" to "resume"
                    doc_type_field => "resume"         // Changed from "parent" to "resume"
                ),
            ])?;

            index_writer.commit()?;
        }
        Ok((index, content_field, doc_type_field))
    }

    #[test]
    fn test_parent_filter_only() -> crate::Result<()> {
        let (index, _content_field, doc_type_field) = create_minimal_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"), // Changed from "parent" to "resume"
            IndexRecordOption::Basic,
        );

        // Just search for parents directly
        let top_docs = searcher.search(&parent_query, &TopDocs::with_limit(1))?;
        assert_eq!(top_docs.len(), 1, "Should find exactly one parent document");

        Ok(())
    }

    #[test]
    fn test_child_query_only() -> crate::Result<()> {
        let (index, _content_field, doc_type_field) = create_minimal_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();

        let child_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "child"),
            IndexRecordOption::Basic,
        );

        // Just search for children directly
        let top_docs = searcher.search(&child_query, &TopDocs::with_limit(1))?;
        assert_eq!(top_docs.len(), 1, "Should find exactly one child document");

        Ok(())
    }

    #[test]
    fn test_parent_bitset_creation() -> crate::Result<()> {
        let (index, _content_field, doc_type_field) = create_minimal_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let segment_reader = searcher.segment_reader(0);

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"), // Changed from "parent" to "resume"
            IndexRecordOption::Basic,
        );

        let parent_weight =
            parent_query.weight(EnableScoring::disabled_from_searcher(&reader.searcher()))?;
        let mut parent_scorer = parent_weight.scorer(segment_reader, 1.0)?;

        let mut parent_docs = Vec::new();
        while parent_scorer.doc() != TERMINATED {
            parent_docs.push(parent_scorer.doc());
            parent_scorer.advance();
        }

        assert_eq!(
            parent_docs.len(),
            1,
            "Should find exactly one parent document"
        );
        assert_eq!(parent_docs[0], 1, "Parent document should be at position 1");

        Ok(())
    }

    #[test]
    fn test_minimal_block_join() -> crate::Result<()> {
        let (index, content_field, doc_type_field) = create_minimal_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"), // Changed from "parent" to "resume"
            IndexRecordOption::Basic,
        );

        let child_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "child"),
            IndexRecordOption::Basic,
        );

        let block_join_query = BlockJoinQuery::new(
            Box::new(child_query),
            Box::new(parent_query),
            ScoreMode::None, // Start with simplest scoring mode
        );

        let top_docs = searcher.search(&block_join_query, &TopDocs::with_limit(1))?;
        assert_eq!(top_docs.len(), 1, "Should find exactly one document");

        let doc: TantivyDocument = searcher.doc(top_docs[0].1)?;
        let content = doc.get_first(content_field).unwrap().as_str().unwrap();
        assert_eq!(content, "resume content", "Should retrieve parent document");

        Ok(())
    }
}

// src/query/block_join_query.rs

#[cfg(test)]
mod atomic_scorer_tests {
    use super::*;
    use crate::collector::TopDocs;
    use crate::query::TermQuery;
    use crate::schema::{Field, IndexRecordOption, Schema, Value, STORED, STRING};
    use crate::{DocAddress, Index, IndexWriter, TantivyDocument, Term};

    /// Creates a test index with a very specific document arrangement for testing scorer behavior
    fn create_scorer_test_index() -> crate::Result<(Index, Field, Field)> {
        let mut schema_builder = Schema::builder();
        let content_field = schema_builder.add_text_field("content", STRING | STORED);
        let doc_type_field = schema_builder.add_text_field("docType", STRING);
        let schema = schema_builder.build();

        let index = Index::create_in_ram(schema);
        {
            let mut index_writer: IndexWriter = index.writer_for_tests()?;

            // Create a very specific arrangement:
            // doc0: child
            // doc1: resume
            // doc2: child
            // doc3: resume
            index_writer.add_documents(vec![
                // First block
                doc!(
                    content_field => "first child",
                    doc_type_field => "child"
                ),
                doc!(
                    content_field => "first resume", // Changed from "parent" to "resume"
                    doc_type_field => "resume"        // Changed from "parent" to "resume"
                ),
                // Second block
                doc!(
                    content_field => "second child",
                    doc_type_field => "child"
                ),
                doc!(
                    content_field => "second resume", // Changed from "parent" to "resume"
                    doc_type_field => "resume"         // Changed from "parent" to "resume"
                ),
            ])?;

            index_writer.commit()?;
        }
        Ok((index, content_field, doc_type_field))
    }

    #[test]
    pub fn test_parent_filter_only() -> crate::Result<()> {
        let (index, _content_field, doc_type_field) = create_scorer_test_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"), // Changed from "parent" to "resume"
            IndexRecordOption::Basic,
        );

        let top_docs = searcher.search(&parent_query, &TopDocs::with_limit(1))?;
        assert_eq!(top_docs.len(), 1, "Should find exactly one parent document");

        Ok(())
    }

    #[test]
    pub fn test_child_query_only() -> crate::Result<()> {
        let (index, _content_field, doc_type_field) = create_scorer_test_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();

        let child_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "child"),
            IndexRecordOption::Basic,
        );

        let top_docs = searcher.search(&child_query, &TopDocs::with_limit(1))?;
        assert_eq!(top_docs.len(), 1, "Should find exactly one child document");

        Ok(())
    }

    #[test]
    pub fn test_parent_bitset_creation() -> crate::Result<()> {
        let (index, _content_field, doc_type_field) = create_scorer_test_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let segment_reader = searcher.segment_reader(0);

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"), // Changed from "parent" to "resume"
            IndexRecordOption::Basic,
        );

        let parent_weight =
            parent_query.weight(EnableScoring::disabled_from_searcher(&searcher))?;
        let mut parent_scorer = parent_weight.scorer(segment_reader, 1.0)?;

        let mut parent_docs = Vec::new();
        while parent_scorer.doc() != TERMINATED {
            parent_docs.push(parent_scorer.doc());
            parent_scorer.advance();
        }

        assert_eq!(
            parent_docs.len(),
            2,
            "Should find exactly two parent documents"
        );
        assert_eq!(
            parent_docs,
            vec![1, 3],
            "Parents should be at positions 1 and 3"
        );

        Ok(())
    }

    #[test]
    pub fn test_minimal_block_join() -> crate::Result<()> {
        let (index, content_field, doc_type_field) = create_scorer_test_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"), // Changed from "parent" to "resume"
            IndexRecordOption::Basic,
        );

        let child_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "child"),
            IndexRecordOption::Basic,
        );

        let block_join_query = BlockJoinQuery::new(
            Box::new(child_query),
            Box::new(parent_query),
            ScoreMode::None, // Start with simplest scoring mode
        );

        let top_docs = searcher.search(&block_join_query, &TopDocs::with_limit(1))?;
        assert_eq!(top_docs.len(), 1, "Should find exactly one document");

        let doc: TantivyDocument = searcher.doc(top_docs[0].1)?;
        let content = doc.get_first(content_field).unwrap().as_str().unwrap();
        assert_eq!(content, "resume content", "Should retrieve parent document");

        Ok(())
    }
}

// src/query/block_join_query.rs

#[cfg(test)]
mod first_advance_tests {
    use super::*;
    use crate::query::TermQuery;
    use crate::schema::{Field, IndexRecordOption, Schema, STRING};
    use crate::{DocAddress, Index, IndexWriter, TantivyDocument, Term};

    /// Creates a minimal test index with exactly one child followed by one parent
    fn create_single_block_index() -> crate::Result<(Index, Field)> {
        let mut schema_builder = Schema::builder();
        let doc_type_field = schema_builder.add_text_field("docType", STRING);
        let schema = schema_builder.build();

        let index = Index::create_in_ram(schema);
        {
            let mut index_writer: IndexWriter = index.writer_for_tests()?;

            // Single block: one child, one parent
            index_writer.add_documents(vec![
                doc!(doc_type_field => "child"),
                doc!(doc_type_field => "resume"), // Changed from "parent" to "resume"
            ])?;

            index_writer.commit()?;
        }
        Ok((index, doc_type_field))
    }

    #[test]
    fn test_first_advance_behavior() -> crate::Result<()> {
        let (index, doc_type_field) = create_single_block_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let segment_reader = searcher.segment_reader(0);

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"), // Changed from "parent" to "resume"
            IndexRecordOption::Basic,
        );
        let child_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "child"),
            IndexRecordOption::Basic,
        );

        let block_join_weight = BlockJoinWeight {
            child_weight: child_query.weight(EnableScoring::disabled_from_searcher(&searcher))?,
            parents_weight: parent_query
                .weight(EnableScoring::disabled_from_searcher(&searcher))?,
            score_mode: ScoreMode::None,
        };

        let mut scorer = block_join_weight.scorer(segment_reader, 1.0)?;

        println!("Initial doc: {}", scorer.doc());

        // First advance should find the parent
        let first_doc = scorer.advance();
        println!("After first advance: {}", first_doc);

        assert_eq!(
            first_doc, 1,
            "First advance should find parent at position 1"
        );

        // Subsequent advance should find TERMINATED
        let next_doc = scorer.advance();
        println!("After second advance: {}", next_doc);

        assert_eq!(
            next_doc, TERMINATED,
            "Second advance should return TERMINATED"
        );

        Ok(())
    }

    #[test]
    fn test_block_join_scoring() -> crate::Result<()> {
        let (index, doc_type_field) = create_single_block_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let segment_reader = searcher.segment_reader(0);

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"), // Changed from "parent" to "resume"
            IndexRecordOption::Basic,
        );
        let child_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "child"),
            IndexRecordOption::Basic,
        );

        let block_join_weight = BlockJoinWeight {
            child_weight: child_query.weight(EnableScoring::disabled_from_searcher(&searcher))?,
            parents_weight: parent_query
                .weight(EnableScoring::disabled_from_searcher(&searcher))?,
            score_mode: ScoreMode::None,
        };

        let mut scorer = block_join_weight.scorer(segment_reader, 1.0)?;

        // Advance to first parent
        let doc = scorer.advance();
        assert_eq!(doc, 1, "Should find parent at position 1");

        // Check the score
        let score = scorer.score();
        assert_eq!(score, 1.0, "Score should be 1.0 with ScoreMode::None");

        Ok(())
    }
}

// src/query/block_join_query.rs

#[cfg(test)]
mod advancement_tests {
    use super::*;
    use crate::query::TermQuery;
    use crate::schema::{Field, IndexRecordOption, Schema, STRING};
    use crate::{DocAddress, Index, IndexWriter, TantivyDocument, Term};

    /// Creates a test index with a specific pattern to test block membership:
    /// doc0: child1
    /// doc1: resume1
    /// doc2: child2
    /// doc3: resume2
    fn create_block_test_index() -> crate::Result<(Index, Field)> {
        let mut schema_builder = Schema::builder();
        let doc_type_field = schema_builder.add_text_field("docType", STRING);
        let schema = schema_builder.build();

        let index = Index::create_in_ram(schema);
        {
            let mut index_writer: IndexWriter = index.writer_for_tests()?;

            // First block
            index_writer.add_documents(vec![
                doc!(doc_type_field => "child"),  // doc0
                doc!(doc_type_field => "resume"), // doc1
                doc!(doc_type_field => "child"),  // doc2
                doc!(doc_type_field => "resume"), // doc3
            ])?;

            index_writer.commit()?;
        }
        Ok((index, doc_type_field))
    }

    #[test]
    fn test_initial_scorer_state() -> crate::Result<()> {
        let (index, doc_type_field) = create_block_test_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let segment_reader = searcher.segment_reader(0);

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"), // Changed from "parent" to "resume"
            IndexRecordOption::Basic,
        );
        let child_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "child"),
            IndexRecordOption::Basic,
        );

        let block_join_weight = BlockJoinWeight {
            child_weight: child_query.weight(EnableScoring::disabled_from_searcher(&searcher))?,
            parents_weight: parent_query
                .weight(EnableScoring::disabled_from_searcher(&searcher))?,
            score_mode: ScoreMode::None,
        };

        let mut scorer = block_join_weight.scorer(segment_reader, 1.0)?;

        // Initial doc should be TERMINATED
        assert_eq!(scorer.doc(), TERMINATED, "Should start at TERMINATED");

        // First advance should find the first parent
        let first_doc = scorer.advance();
        assert_eq!(
            first_doc, 1,
            "First advance should find parent at position 1"
        );

        // Second advance should find the second parent
        let second_doc = scorer.advance();
        assert_eq!(
            second_doc, 3,
            "Second advance should find parent at position 3"
        );

        // Third advance should return TERMINATED
        let third_doc = scorer.advance();
        assert_eq!(
            third_doc, TERMINATED,
            "Third advance should return TERMINATED"
        );

        Ok(())
    }

    #[test]
    fn test_block_join_scoring() -> crate::Result<()> {
        let (index, doc_type_field) = create_block_test_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let segment_reader = searcher.segment_reader(0);

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"), // Changed from "parent" to "resume"
            IndexRecordOption::Basic,
        );
        let child_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "child"),
            IndexRecordOption::Basic,
        );

        let block_join_weight = BlockJoinWeight {
            child_weight: child_query.weight(EnableScoring::disabled_from_searcher(&searcher))?,
            parents_weight: parent_query
                .weight(EnableScoring::disabled_from_searcher(&searcher))?,
            score_mode: ScoreMode::None,
        };

        let mut scorer = block_join_weight.scorer(segment_reader, 1.0)?;

        // Advance to first parent
        let first_doc = scorer.advance();
        assert_eq!(first_doc, 1, "Should find parent at position 1");

        // Check the score
        let score = scorer.score();
        assert_eq!(score, 1.0, "Score should be 1.0 with ScoreMode::None");

        // Advance to second parent
        let second_doc = scorer.advance();
        assert_eq!(second_doc, 3, "Should find parent at position 3");

        // Check the score
        let score = scorer.score();
        assert_eq!(score, 1.0, "Score should be 1.0 with ScoreMode::None");

        Ok(())
    }
}

#[cfg(test)]
mod block_membership_tests {
    use super::*;
    use crate::collector::TopDocs;
    use crate::query::TermQuery;
    use crate::schema::{Field, IndexRecordOption, Schema, STRING};
    use crate::{Index, IndexWriter, Term};

    /// Creates a test index with a specific pattern to test block membership:
    /// doc0: child1
    /// doc1: child2
    /// doc2: resume1
    /// doc3: child3
    /// doc4: resume2
    fn create_block_test_index() -> crate::Result<(Index, Field)> {
        let mut schema_builder = Schema::builder();
        let doc_type_field = schema_builder.add_text_field("docType", STRING);
        let schema = schema_builder.build();

        let index = Index::create_in_ram(schema);
        {
            let mut index_writer: IndexWriter = index.writer_for_tests()?;

            // First block
            index_writer.add_documents(vec![
                doc!(doc_type_field => "child"),  // doc0
                doc!(doc_type_field => "child"),  // doc1
                doc!(doc_type_field => "resume"), // doc2
            ])?;

            // Second block
            index_writer.add_documents(vec![
                doc!(doc_type_field => "child"),  // doc3
                doc!(doc_type_field => "resume"), // doc4
            ])?;

            index_writer.commit()?;
        }
        Ok((index, doc_type_field))
    }

    #[test]
    fn test_child_block_membership() -> crate::Result<()> {
        let (index, doc_type_field) = create_block_test_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let segment_reader = searcher.segment_reader(0);

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"),
            IndexRecordOption::Basic,
        );
        let child_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "child"),
            IndexRecordOption::Basic,
        );

        let block_join_weight = BlockJoinWeight {
            child_weight: child_query.weight(EnableScoring::disabled_from_searcher(&searcher))?,
            parents_weight: parent_query
                .weight(EnableScoring::disabled_from_searcher(&searcher))?,
            score_mode: ScoreMode::None,
        };

        let mut scorer = block_join_weight.scorer(segment_reader, 1.0)?;

        // Get first parent
        let first_doc = scorer.advance();
        assert_eq!(first_doc, 2, "First parent should be at position 2");
        assert_eq!(
            scorer.score(),
            1.0,
            "Score should be 1.0 with ScoreMode::None"
        );

        // Get second parent
        let second_doc = scorer.advance();
        assert_eq!(second_doc, 4, "Second parent should be at position 4");
        assert_eq!(
            scorer.score(),
            1.0,
            "Score should be 1.0 with ScoreMode::None"
        );

        Ok(())
    }

    #[test]
    fn test_collect_matches_block_boundaries() -> crate::Result<()> {
        let (index, doc_type_field) = create_block_test_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"),
            IndexRecordOption::Basic,
        );
        let child_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "child"),
            IndexRecordOption::Basic,
        );

        // First verify parents are correctly indexed
        let parent_docs = searcher.search(&parent_query, &TopDocs::with_limit(10))?;
        println!(
            "\n=== Parent documents found directly ({} results) ===",
            parent_docs.len()
        );
        for (i, doc) in parent_docs.iter().enumerate() {
            println!("Parent {}: doc_id={}, score={}", i, doc.1.doc_id, doc.0);
        }

        // Test the block join query scoring
        println!("\n=== Testing block join query ===");
        let block_join_query = BlockJoinQuery::new(
            Box::new(child_query.clone()),
            Box::new(parent_query.clone()),
            ScoreMode::None,
        );

        let collector = TopDocs::with_limit(10);
        println!("\n=== Searching with collector (limit: 10) ===");

        let top_docs = searcher.search(&block_join_query, &collector)?;
        println!(
            "\n=== Search completed, found {} results ===",
            top_docs.len()
        );

        for (i, doc) in top_docs.iter().enumerate() {
            println!("Result {}: doc_id={}, score={}", i, doc.1.doc_id, doc.0);
        }

        println!("\nAsserting results...");
        assert_eq!(top_docs.len(), 2, "Should find both parent documents");

        let mut result_ids: Vec<DocId> = top_docs.iter().map(|(_score, doc)| doc.doc_id).collect();
        result_ids.sort_unstable();
        println!("Sorted result IDs: {:?}", result_ids);

        assert_eq!(result_ids[0], 2, "Should find parent at position 2");
        assert_eq!(result_ids[1], 4, "Should find parent at position 4");

        Ok(())
    }

    #[test]
    fn test_scorer_behavior() -> crate::Result<()> {
        let (index, doc_type_field) = create_block_test_index()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let segment_reader = searcher.segment_reader(0);

        let parent_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "resume"),
            IndexRecordOption::Basic,
        );
        let child_query = TermQuery::new(
            Term::from_field_text(doc_type_field, "child"),
            IndexRecordOption::Basic,
        );

        println!("\n=== Creating block join scorer ===");
        let block_join_query = BlockJoinQuery::new(
            Box::new(child_query),
            Box::new(parent_query),
            ScoreMode::None,
        );

        let weight = block_join_query.weight(EnableScoring::disabled_from_searcher(&searcher))?;
        let mut scorer = weight.scorer(segment_reader, 1.0)?;

        println!("\n=== Testing scorer directly ===");
        let mut docs = Vec::new();
        let initial_doc = scorer.doc();
        println!("Initial doc: {}", initial_doc);

        // First advance
        let mut current = scorer.advance();
        println!("First advance: {}", current);

        while current != TERMINATED {
            docs.push(current);
            current = scorer.advance();
            println!("Advanced to: {}", current);
        }

        println!("\nCollected docs: {:?}", docs);
        assert!(!docs.is_empty(), "Scorer should find documents");
        assert_eq!(docs.len(), 2, "Should find both parents");
        assert_eq!(docs[0], 2, "First parent should be at position 2");
        assert_eq!(docs[1], 4, "Second parent should be at position 4");

        Ok(())
    }
}
