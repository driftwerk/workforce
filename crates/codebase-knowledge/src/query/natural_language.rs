use std::collections::HashSet;

/// A structured retrieval plan generated from a natural language question.
/// The query engine converts a question into one of these, then executes each step.
#[derive(Debug)]
pub struct RetrievalPlan {
    /// Primary search queries to execute (entity name patterns).
    pub entity_patterns: Vec<String>,
    /// Relationship traversals to perform after finding root entities.
    pub graph_traversals: Vec<TraversalStep>,
    /// Whether to perform live code verification (for thorough mode).
    pub verify_live: bool,
    /// Entity kinds to prioritise in results.
    pub preferred_kinds: Vec<String>,
    /// Whether this is an "explanation" question ("how does X work", "explain
    /// …"). Drives the seed-relevance kind score (behaviour-bearing kinds lead)
    /// where the `preferred_kinds` keyword heuristic is empty for prose questions
    /// that name no kind. Set from the `QuestionType` classification.
    pub is_explanation: bool,
}

/// A graph traversal step in the retrieval plan.
#[derive(Debug)]
pub struct TraversalStep {
    pub relationship: String,
    pub max_depth: usize,
}

/// Analyses a natural language question and produces a structured retrieval plan.
/// Uses keyword extraction and question-type classification to determine
/// the most effective graph queries and search patterns.
pub struct QueryPlanner;

impl QueryPlanner {
    /// Analyse a question and produce a retrieval plan.
    pub fn plan(question: &str, thorough: bool) -> RetrievalPlan {
        let question_lower = question.to_lowercase();
        let tokens = Self::tokenize(&question_lower);

        let question_type = Self::classify_question(&question_lower, &tokens);
        let entity_patterns = Self::extract_entity_patterns(question, &tokens);
        let graph_traversals = Self::plan_traversals(&question_type, thorough);
        let preferred_kinds = Self::infer_preferred_kinds(&question_lower);
        let is_explanation = matches!(question_type, QuestionType::Explanation);

        RetrievalPlan {
            entity_patterns,
            graph_traversals,
            verify_live: thorough,
            preferred_kinds,
            is_explanation,
        }
    }

    /// Tokenize the question into meaningful terms, filtering stop words.
    fn tokenize(text: &str) -> Vec<String> {
        let stop_words: HashSet<&str> = [
            "a",
            "an",
            "the",
            "is",
            "are",
            "was",
            "were",
            "be",
            "been",
            "being",
            "have",
            "has",
            "had",
            "do",
            "does",
            "did",
            "will",
            "would",
            "shall",
            "should",
            "may",
            "might",
            "must",
            "can",
            "could",
            "about",
            "above",
            "after",
            "again",
            "all",
            "am",
            "and",
            "any",
            "as",
            "at",
            "below",
            "between",
            "both",
            "but",
            "by",
            "down",
            "during",
            "each",
            "few",
            "for",
            "from",
            "further",
            "get",
            "got",
            "he",
            "her",
            "here",
            "hers",
            "herself",
            "him",
            "himself",
            "his",
            "how",
            "i",
            "if",
            "in",
            "into",
            "it",
            "its",
            "itself",
            "just",
            "me",
            "more",
            "most",
            "my",
            "myself",
            "no",
            "nor",
            "not",
            "now",
            "of",
            "off",
            "on",
            "once",
            "only",
            "or",
            "other",
            "our",
            "ours",
            "ourselves",
            "out",
            "over",
            "own",
            "same",
            "she",
            "so",
            "some",
            "such",
            "than",
            "that",
            "their",
            "theirs",
            "them",
            "themselves",
            "then",
            "there",
            "these",
            "they",
            "this",
            "those",
            "through",
            "to",
            "too",
            "under",
            "until",
            "up",
            "very",
            "we",
            "what",
            "when",
            "where",
            "which",
            "while",
            "who",
            "whom",
            "why",
            "with",
            "you",
            "your",
            "yours",
            "yourself",
            "yourselves",
        ]
        .into_iter()
        .collect();

        text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '.')
            .filter(|t| !t.is_empty() && !stop_words.contains(t))
            .map(|t| t.to_string())
            .collect()
    }

    /// Classify the question type to guide retrieval strategy.
    fn classify_question(question: &str, _tokens: &[String]) -> QuestionType {
        if question.contains("how does")
            || question.contains("how do")
            || question.contains("explain")
        {
            QuestionType::Explanation
        } else if question.contains("what calls")
            || question.contains("who calls")
            || question.contains("callers")
        {
            QuestionType::Callers
        } else if question.contains("what does") && question.contains("call") {
            QuestionType::Callees
        } else if question.contains("import") || question.contains("depend") {
            QuestionType::Dependencies
        } else if question.contains("implement") {
            QuestionType::Implementations
        } else if question.contains("test") {
            QuestionType::Tests
        } else if question.contains("where is")
            || question.contains("find")
            || question.contains("locate")
        {
            QuestionType::Location
        } else if question.contains("what would break")
            || question.contains("impact")
            || question.contains("affect")
            || question.contains("change")
        {
            QuestionType::Impact
        } else if question.contains("architecture")
            || question.contains("overview")
            || question.contains("structure")
        {
            QuestionType::Architecture
        } else {
            QuestionType::General
        }
    }

    /// Extract entity name patterns from the question.
    /// Looks for quoted strings, PascalCase names, snake_case names, and path-like patterns.
    fn extract_entity_patterns(question: &str, tokens: &[String]) -> Vec<String> {
        let mut patterns = Vec::new();

        // Extract quoted strings as exact entity names
        let mut in_quote = false;
        let mut current_quote = String::new();
        let mut quote_char = '"';
        for ch in question.chars() {
            if !in_quote && (ch == '"' || ch == '\'' || ch == '`') {
                in_quote = true;
                quote_char = ch;
                current_quote.clear();
            } else if in_quote && ch == quote_char {
                in_quote = false;
                if !current_quote.is_empty() {
                    patterns.push(current_quote.clone());
                }
            } else if in_quote {
                current_quote.push(ch);
            }
        }

        // Extract PascalCase identifiers (e.g., CodeGraph, EntityId)
        for word in question.split_whitespace() {
            let clean: String = word
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if clean.len() >= 2 {
                let chars: Vec<char> = clean.chars().collect();
                let has_upper_lower = chars.iter().any(|c| c.is_uppercase())
                    && chars.iter().any(|c| c.is_lowercase());
                let starts_upper = chars[0].is_uppercase();

                // Keep a PascalCase word only if it is ALSO a content token
                // (the stop-word-filtered `tokens`). This rejects sentence words
                // like "How"/"What"/"Does" that match the PascalCase shape but are
                // not entities — left in, they poison the search (`LIKE '%How%'`
                // → Show/show) and the content terms never get a chance.
                if starts_upper
                    && has_upper_lower
                    && clean.len() >= 3
                    && tokens.iter().any(|t| t.eq_ignore_ascii_case(&clean))
                {
                    patterns.push(clean);
                }
            }
        }

        // Extract snake_case identifiers
        for word in question.split_whitespace() {
            let clean: String = word
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if clean.contains('_') && clean.len() >= 3 {
                patterns.push(clean);
            }
        }

        // Extract path-like patterns (containing / or ::)
        for word in question.split_whitespace() {
            let clean: String = word
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '/' || *c == ':' || *c == '.')
                .collect();
            if (clean.contains('/') || clean.contains("::")) && clean.len() >= 3 {
                patterns.push(clean);
            }
        }

        // ALWAYS include the meaningful (stop-word-filtered) content tokens, not
        // only as an `is_empty` fallback. A prose question ("how does the
        // scheduler work") must yield its content terms ("scheduler") even when a
        // stray PascalCase/snake_case pattern is also present — otherwise a single
        // bogus pattern suppresses the real search terms entirely.
        for token in tokens {
            if token.len() >= 3 {
                patterns.push(token.clone());
            }
        }

        // Deduplicate while preserving order
        let mut seen = HashSet::new();
        patterns.retain(|p| seen.insert(p.clone()));

        patterns
    }

    /// Plan graph traversals based on question type.
    fn plan_traversals(question_type: &QuestionType, thorough: bool) -> Vec<TraversalStep> {
        let depth = if thorough { 3 } else { 2 };

        match question_type {
            QuestionType::Callers => vec![TraversalStep {
                relationship: "called_by".to_string(),
                max_depth: depth,
            }],
            QuestionType::Callees => vec![TraversalStep {
                relationship: "calls".to_string(),
                max_depth: depth,
            }],
            QuestionType::Dependencies => vec![
                TraversalStep {
                    relationship: "imports".to_string(),
                    max_depth: depth,
                },
                TraversalStep {
                    relationship: "depends_on".to_string(),
                    max_depth: depth,
                },
            ],
            QuestionType::Implementations => vec![TraversalStep {
                relationship: "implements".to_string(),
                max_depth: 1,
            }],
            QuestionType::Tests => vec![TraversalStep {
                relationship: "tests_for".to_string(),
                max_depth: 1,
            }],
            QuestionType::Explanation => vec![
                TraversalStep {
                    relationship: "calls".to_string(),
                    max_depth: depth,
                },
                TraversalStep {
                    relationship: "called_by".to_string(),
                    max_depth: 1,
                },
                TraversalStep {
                    relationship: "imports".to_string(),
                    max_depth: 1,
                },
            ],
            QuestionType::Impact => vec![
                TraversalStep {
                    relationship: "called_by".to_string(),
                    max_depth: depth,
                },
                TraversalStep {
                    relationship: "imports".to_string(),
                    max_depth: depth,
                },
                TraversalStep {
                    relationship: "implements".to_string(),
                    max_depth: 1,
                },
            ],
            QuestionType::Location | QuestionType::Architecture | QuestionType::General => vec![],
        }
    }

    /// Infer which entity kinds to prioritise based on question content.
    fn infer_preferred_kinds(question: &str) -> Vec<String> {
        let mut kinds = Vec::new();
        if question.contains("function") || question.contains("fn ") || question.contains("method")
        {
            kinds.push("function".to_string());
            kinds.push("method".to_string());
        }
        if question.contains("struct") || question.contains("class") || question.contains("type") {
            kinds.push("struct".to_string());
            kinds.push("class".to_string());
        }
        if question.contains("trait") || question.contains("interface") {
            kinds.push("trait".to_string());
            kinds.push("interface".to_string());
        }
        if question.contains("module") || question.contains("file") {
            kinds.push("module".to_string());
            kinds.push("file".to_string());
        }
        if question.contains("test") {
            kinds.push("test".to_string());
        }
        kinds
    }
}

/// Classification of a question to guide retrieval strategy.
#[derive(Debug)]
enum QuestionType {
    Explanation,
    Callers,
    Callees,
    Dependencies,
    Implementations,
    Tests,
    Location,
    Impact,
    Architecture,
    General,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_extracts_quoted_names() {
        let plan = QueryPlanner::plan("How does `CodeGraph` work?", false);
        assert!(plan.entity_patterns.contains(&"CodeGraph".to_string()));
    }

    #[test]
    fn plan_extracts_pascal_case() {
        let plan = QueryPlanner::plan("What does EntityId do?", false);
        assert!(plan.entity_patterns.contains(&"EntityId".to_string()));
    }

    #[test]
    fn plan_extracts_snake_case() {
        let plan = QueryPlanner::plan("Where is find_entities_by_name defined?", false);
        assert!(plan
            .entity_patterns
            .contains(&"find_entities_by_name".to_string()));
    }

    #[test]
    fn plan_caller_question_traverses_called_by() {
        let plan = QueryPlanner::plan("What calls process_message?", false);
        assert!(!plan.graph_traversals.is_empty());
        assert_eq!(plan.graph_traversals[0].relationship, "called_by");
    }

    #[test]
    fn thorough_mode_increases_depth() {
        let quick = QueryPlanner::plan("What calls foo?", false);
        let thorough = QueryPlanner::plan("What calls foo?", true);
        assert!(thorough.verify_live);
        assert!(!quick.verify_live);
        if !quick.graph_traversals.is_empty() && !thorough.graph_traversals.is_empty() {
            assert!(thorough.graph_traversals[0].max_depth >= quick.graph_traversals[0].max_depth);
        }
    }

    // REGRESSION (do not delete): a prose question must yield its CONTENT terms,
    // never the sentence word "How". A `git checkout` once silently reverted the
    // fix and dropped these tests; their absence is why it went unnoticed.
    #[test]
    fn plan_prose_scheduler_yields_content_tokens_not_how() {
        let plan = QueryPlanner::plan("How does the scheduler work in workforce?", false);
        assert!(
            plan.entity_patterns.iter().any(|p| p == "scheduler"),
            "must search for 'scheduler'; got {:?}",
            plan.entity_patterns
        );
        assert!(
            !plan
                .entity_patterns
                .iter()
                .any(|p| p.eq_ignore_ascii_case("how")),
            "must NOT search for the sentence word 'How'; got {:?}",
            plan.entity_patterns
        );
    }

    #[test]
    fn plan_prose_vault_yields_vault_and_encrypt() {
        let plan = QueryPlanner::plan("How does the secret vault encrypt secrets?", false);
        assert!(
            plan.entity_patterns.iter().any(|p| p == "vault"),
            "must search for 'vault'; got {:?}",
            plan.entity_patterns
        );
        assert!(
            plan.entity_patterns.iter().any(|p| p == "encrypt"),
            "must search for 'encrypt'; got {:?}",
            plan.entity_patterns
        );
    }

    #[test]
    fn plan_keeps_pascal_case_type_even_in_prose() {
        // A real type that IS a content token survives (vs "How" which doesn't).
        let plan = QueryPlanner::plan("How does CodeGraph index a repo?", false);
        assert!(plan.entity_patterns.contains(&"CodeGraph".to_string()));
        assert!(!plan
            .entity_patterns
            .iter()
            .any(|p| p.eq_ignore_ascii_case("how")));
    }
}
