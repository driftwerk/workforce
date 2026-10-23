//! The wire types of `POST /v1/systemone`, as TypeSafe documents them and Kev
//! reproduces them.

use crate::client::DecideError;
use crate::text;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// One question about the state. `instructions` and every criterion may be a
/// string or JSON structure; the model reads either.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    /// A yes/no question. The answer is the probability of yes.
    Noul {
        instructions: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    /// Pick one option. Keys are the option names; values describe them
    /// (`null` for none). At most 255 options.
    Choice {
        instructions: Value,
        criteria: BTreeMap<String, Value>,
    },
    /// Rate against ordered levels, lowest first. Two to ten levels.
    Score {
        instructions: Value,
        criteria: Vec<Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct NoulCriteria {
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    pub yes: Option<Value>,
    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    pub no: Option<Value>,
}

impl Question {
    pub fn noul(instructions: impl Into<Value>) -> Self {
        Question::Noul {
            instructions: instructions.into(),
            criteria: None,
        }
    }

    pub fn choice(
        instructions: impl Into<Value>,
        options: impl IntoIterator<Item = (impl Into<String>, impl Into<Value>)>,
    ) -> Self {
        Question::Choice {
            instructions: instructions.into(),
            criteria: options
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    pub fn score(
        instructions: impl Into<Value>,
        levels: impl IntoIterator<Item = impl Into<Value>>,
    ) -> Self {
        Question::Score {
            instructions: instructions.into(),
            criteria: levels.into_iter().map(Into::into).collect(),
        }
    }
}

impl Question {
    /// What a model reads of this question and a longer one can lose some
    /// of: its instructions, then its criteria.
    fn values(&self) -> Vec<&Value> {
        let instructions = std::iter::once(match self {
            Question::Noul { instructions, .. }
            | Question::Choice { instructions, .. }
            | Question::Score { instructions, .. } => instructions,
        });
        match self {
            Question::Noul { criteria, .. } => instructions
                .chain(criteria.iter().flat_map(|c| [&c.yes, &c.no]).flatten())
                .collect(),
            Question::Choice { criteria, .. } => instructions.chain(criteria.values()).collect(),
            Question::Score { criteria, .. } => instructions.chain(criteria).collect(),
        }
    }

    /// This question with `values` in place of its own, in the order
    /// [`Self::values`] gives them.
    fn with_values(&self, values: &mut impl Iterator<Item = Value>) -> Self {
        let mut next = |old: &Value| values.next().unwrap_or_else(|| old.clone());
        match self {
            Question::Noul {
                instructions,
                criteria,
            } => Question::Noul {
                instructions: next(instructions),
                criteria: criteria.as_ref().map(|c| NoulCriteria {
                    yes: c.yes.as_ref().map(&mut next),
                    no: c.no.as_ref().map(&mut next),
                }),
            },
            Question::Choice {
                instructions,
                criteria,
            } => Question::Choice {
                instructions: next(instructions),
                criteria: criteria
                    .iter()
                    .map(|(name, meaning)| (name.clone(), next(meaning)))
                    .collect(),
            },
            Question::Score {
                instructions,
                criteria,
            } => Question::Score {
                instructions: next(instructions),
                criteria: criteria.iter().map(next).collect(),
            },
        }
    }

    /// The characters of a choice's option names: read by the model, and
    /// never shortened, since an answer comes back under them.
    fn names(&self) -> usize {
        match self {
            Question::Choice { criteria, .. } => {
                criteria.keys().map(|name| name.chars().count()).sum()
            }
            _ => 0,
        }
    }

    /// The characters of text this question adds to a request.
    pub fn chars(&self) -> usize {
        self.names() + self.values().into_iter().map(text::chars).sum::<usize>()
    }
}

/// The characters of text `questions` add to a request.
pub fn chars(questions: &BTreeMap<String, Question>) -> usize {
    questions.values().map(Question::chars).sum()
}

/// `questions` within `limit` characters. Questions that fit are returned
/// as written; otherwise their criteria share what the option names and
/// the instructions leave, the short ones whole and the long ones cut to
/// their opening. What is asked is never cut: a question whose names and
/// instructions alone pass `limit` keeps both and loses its criteria.
pub fn fit(questions: BTreeMap<String, Question>, limit: usize) -> BTreeMap<String, Question> {
    if chars(&questions) <= limit {
        return questions;
    }
    let asked = |question: &Question| question.names() + text::chars(question.values()[0]);
    let kept: usize = questions.values().map(asked).sum();
    let criteria: Vec<&Value> = questions
        .values()
        .flat_map(|question| question.values().into_iter().skip(1))
        .collect();
    let mut fitted = text::fit_values(&criteria, limit.saturating_sub(kept)).into_iter();
    questions
        .iter()
        .map(|(id, question)| {
            let values = question.values();
            let own: Vec<Value> = std::iter::once(values[0].clone())
                .chain(fitted.by_ref().take(values.len() - 1))
                .collect();
            (id.clone(), question.with_values(&mut own.into_iter()))
        })
        .collect()
}

/// One answer, under the same id as its question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        /// Probability of yes, 0 to 1.
        noul: f64,
    },
    Choice {
        /// The highest-probability option.
        choice: String,
        /// Every option's probability; they sum to 1.
        probabilities: BTreeMap<String, f64>,
        /// How peaked the distribution is, 0 to 1. Not a hit rate.
        confidence: f64,
    },
    Score {
        /// Probability-weighted level index; may fall between levels.
        score: f64,
        /// Level index (as a string) to its description.
        #[serde(default)]
        legend: BTreeMap<String, String>,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
}

impl Answer {
    /// The probability the model put on the option it chose (Choice), on yes
    /// (Noul), or on the most likely level (Score). The number to threshold
    /// on: independent reviews found the `confidence` field no more reliable
    /// than this, and sometimes less.
    pub fn top_probability(&self) -> f64 {
        match self {
            Answer::Noul { noul } => *noul,
            Answer::Choice {
                choice,
                probabilities,
                ..
            } => probabilities.get(choice).copied().unwrap_or(0.0),
            Answer::Score { probabilities, .. } => {
                probabilities.values().copied().fold(0.0, f64::max)
            }
        }
    }

    /// A short label for the answer, for comparing two backends: the chosen
    /// option, `yes`/`no` at 0.5, or the most likely level.
    pub fn verdict(&self) -> String {
        match self {
            Answer::Noul { noul } => if *noul >= 0.5 { "yes" } else { "no" }.to_string(),
            Answer::Choice { choice, .. } => choice.clone(),
            Answer::Score { probabilities, .. } => probabilities
                .iter()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(level, _)| level.clone())
                .unwrap_or_default(),
        }
    }
}

/// Several question sets about one state as the questions of one request,
/// so things asked of the same state cost one call. Ids must be distinct
/// across the sets: each answer comes back under its question's id.
pub fn batch(
    sets: impl IntoIterator<Item = BTreeMap<String, Question>>,
) -> Result<BTreeMap<String, Question>, DecideError> {
    sets.into_iter()
        .flatten()
        .try_fold(BTreeMap::new(), |mut all, (id, question)| {
            match all.insert(id.clone(), question) {
                Some(_) => Err(DecideError::DuplicateQuestion { id }),
                None => Ok(all),
            }
        })
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// The versioned model that answered, e.g. `jev-1.13.0` or `kev-latest`.
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
    #[serde(default)]
    pub usage: Usage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn questions_serialise_to_the_documented_shape() {
        let q = Question::choice(
            "Which team?",
            [("billing", json!("Payments")), ("technical", Value::Null)],
        );
        assert_eq!(
            serde_json::to_value(&q).unwrap(),
            json!({"type":"choice","instructions":"Which team?","criteria":{"billing":"Payments","technical":null}})
        );
        let n = Question::noul("Urgent?");
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            json!({"type":"noul","instructions":"Urgent?"})
        );
        let s = Question::score("How bad?", ["calm", "annoyed"]);
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!({"type":"score","instructions":"How bad?","criteria":["calm","annoyed"]})
        );
    }

    #[test]
    fn question_sets_batch_into_one_and_a_repeated_id_is_refused() {
        let scope = BTreeMap::from([("on_topic".to_string(), Question::noul("On topic?"))]);
        let routing = BTreeMap::from([("use_case".to_string(), Question::noul("Code?"))]);
        let all = batch([scope.clone(), routing]).unwrap();
        assert_eq!(all.keys().collect::<Vec<_>>(), ["on_topic", "use_case"]);
        assert!(matches!(
            batch([scope.clone(), scope]),
            Err(DecideError::DuplicateQuestion { id }) if id == "on_topic"
        ));
    }

    #[test]
    fn answers_parse_and_summarise() {
        let r: Response = serde_json::from_value(json!({
            "model": "jev-1.13.0",
            "answers": {
                "team": {"type":"choice","choice":"billing","probabilities":{"billing":0.88,"technical":0.12},"confidence":0.81},
                "urgent": {"type":"noul","noul":0.95},
                "mood": {"type":"score","score":1.05,"legend":{"0":"Calm","1":"Frustrated"},"probabilities":{"0":0.05,"1":0.95},"confidence":0.92}
            },
            "usage": {"input_tokens": 296, "output_tokens": 20}
        }))
        .unwrap();
        assert_eq!(r.answers["team"].verdict(), "billing");
        assert!((r.answers["team"].top_probability() - 0.88).abs() < 1e-9);
        assert_eq!(r.answers["urgent"].verdict(), "yes");
        assert_eq!(r.answers["mood"].verdict(), "1");
        assert!((r.answers["mood"].top_probability() - 0.95).abs() < 1e-9);
        assert_eq!(r.usage.input_tokens, 296);
    }
}
