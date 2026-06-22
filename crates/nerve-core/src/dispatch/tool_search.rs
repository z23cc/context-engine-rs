use super::{ToolText, args::ToolSearchArgs};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Debug, Serialize)]
pub(super) struct ToolSearchResponse {
    query: String,
    total_tools: usize,
    matched_tools: usize,
    matches: Vec<ToolSearchMatch>,
}

#[derive(Debug, Serialize)]
pub(super) struct ToolSearchMatch {
    name: String,
    score: usize,
    description: String,
    required: Vec<String>,
    parameters: Vec<ToolParameter>,
    matched_terms: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ToolParameter {
    name: String,
    required: bool,
    description: Option<String>,
}

pub(super) fn search_tool_specs(args: ToolSearchArgs) -> ToolSearchResponse {
    let specs = super::specs::tool_specs();
    let tools = specs.as_array().cloned().unwrap_or_default();
    let terms = query_terms(&args.query);
    let mut matches = tools
        .iter()
        .filter_map(|tool| tool_match(tool, &terms))
        .collect::<Vec<_>>();
    matches.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
    let matched_tools = matches.len();
    matches.truncate(args.max_results);
    ToolSearchResponse {
        query: args.query,
        total_tools: tools.len(),
        matched_tools,
        matches,
    }
}

impl ToolText for ToolSearchResponse {
    fn tool_text(&self) -> String {
        if self.matches.is_empty() {
            return format!(
                "tool_search: no matches for {:?} ({} tools searched)\n",
                self.query, self.total_tools
            );
        }
        let mut out = format!(
            "tool_search: {} of {} tools matched {:?}\n",
            self.matched_tools, self.total_tools, self.query
        );
        for item in &self.matches {
            out.push_str(&format!(
                "  {} (score {}): {}\n",
                item.name, item.score, item.description
            ));
            if !item.parameters.is_empty() {
                out.push_str("    params: ");
                let params = item
                    .parameters
                    .iter()
                    .take(8)
                    .map(|param| {
                        if param.required {
                            format!("{}*", param.name)
                        } else {
                            param.name.clone()
                        }
                    })
                    .collect::<Vec<_>>();
                out.push_str(&params.join(", "));
                if item.parameters.len() > params.len() {
                    out.push_str(", …");
                }
                out.push('\n');
            }
        }
        out
    }
}

fn tool_match(tool: &Value, terms: &[String]) -> Option<ToolSearchMatch> {
    let name = tool.get("name")?.as_str()?.to_string();
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let required = required_params(tool);
    let parameters = parameters(tool, &required);
    let mut matched_terms = Vec::new();
    let mut score = 0usize;
    for term in terms {
        let term_score = score_term(term, &name, &description, &parameters);
        if term_score > 0 {
            score += term_score;
            matched_terms.push(term.clone());
        }
    }
    (score > 0).then_some(ToolSearchMatch {
        name,
        score,
        description,
        required,
        parameters,
        matched_terms,
    })
}

fn query_terms(query: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(str::to_ascii_lowercase)
        .filter(|term| seen.insert(term.clone()))
        .collect()
}

fn score_term(term: &str, name: &str, description: &str, parameters: &[ToolParameter]) -> usize {
    let mut score = field_score(term, name, 40) + field_score(term, description, 12);
    for param in parameters {
        score += field_score(term, &param.name, 8);
        if let Some(description) = &param.description {
            score += field_score(term, description, 4);
        }
    }
    score
}

fn field_score(term: &str, field: &str, weight: usize) -> usize {
    let lower = field.to_ascii_lowercase();
    if lower == term {
        return weight * 3;
    }
    if lower
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|token| token == term)
    {
        return weight;
    }
    if lower.contains(term) { weight / 2 } else { 0 }
}

fn required_params(tool: &Value) -> Vec<String> {
    let mut required = tool
        .pointer("/inputSchema/required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    required.sort();
    required
}

fn parameters(tool: &Value, required: &[String]) -> Vec<ToolParameter> {
    let required: BTreeSet<&str> = required.iter().map(String::as_str).collect();
    let Some(properties) = tool
        .pointer("/inputSchema/properties")
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    let mut params = properties
        .iter()
        .map(|(name, schema)| ToolParameter {
            name: name.clone(),
            required: required.contains(name.as_str()),
            description: schema
                .get("description")
                .and_then(Value::as_str)
                .map(ToString::to_string),
        })
        .collect::<Vec<_>>();
    params.sort_by(|a, b| {
        b.required
            .cmp(&a.required)
            .then_with(|| a.name.cmp(&b.name))
    });
    params
}
