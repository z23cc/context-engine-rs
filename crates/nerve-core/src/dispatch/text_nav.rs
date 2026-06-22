use super::ToolText;

impl ToolText for crate::navigate::FindReferencingSymbolsResponse {
    fn tool_text(&self) -> String {
        let mut out = format!(
            "find_referencing_symbols: {} referencing symbols for {}\n",
            self.total, self.symbol
        );
        if !self.definitions.is_empty() {
            out.push_str("definitions:\n");
            for def in &self.definitions {
                match &def.signature {
                    Some(signature) => out.push_str(&format!(
                        "  {}:{}:{} {} {}\n",
                        def.display_path, def.line, def.column, def.kind, signature
                    )),
                    None => out.push_str(&format!(
                        "  {}:{}:{} {}\n",
                        def.display_path, def.line, def.column, def.kind
                    )),
                }
            }
        }
        if self.referencing_symbols.is_empty() {
            out.push_str("(no referencing symbols)\n");
            return out;
        }
        if self.definition_count > 1 {
            out.push_str(&format!(
                "referencing symbols ({} definitions of this name — low-confidence may be unrelated):\n",
                self.definition_count
            ));
        } else {
            out.push_str("referencing symbols:\n");
        }
        for item in &self.referencing_symbols {
            let confidence = match item.confidence {
                crate::navigate::Confidence::High => "",
                crate::navigate::Confidence::Low => " [low]",
            };
            out.push_str(&format!(
                "  {}:{}:{} {} references at {}:{} ({}){}\n",
                item.display_path,
                item.line,
                item.column,
                item.symbol,
                item.reference_line,
                item.reference_column,
                item.reference_kind,
                confidence
            ));
        }
        if self.truncated {
            out.push_str(&format!(
                "(showing {} of {})\n",
                self.referencing_symbols.len(),
                self.total
            ));
        }
        out
    }
}

impl ToolText for crate::navigate::ImpactAnalysisResponse {
    fn tool_text(&self) -> String {
        let mut out = format!(
            "analyze_impact: {} impacted enclosing symbols for {} (max_depth {})\n",
            self.total, self.symbol, self.max_depth
        );
        if !self.definitions.is_empty() {
            out.push_str("definitions:\n");
            for def in &self.definitions {
                match &def.signature {
                    Some(signature) => out.push_str(&format!(
                        "  {}:{}:{} {} {}\n",
                        def.display_path, def.line, def.column, def.kind, signature
                    )),
                    None => out.push_str(&format!(
                        "  {}:{}:{} {}\n",
                        def.display_path, def.line, def.column, def.kind
                    )),
                }
            }
        }
        if self.impacted.is_empty() {
            out.push_str("(no impacted symbols)\n");
            return out;
        }
        out.push_str("impacted:\n");
        for item in &self.impacted {
            let confidence = match item.confidence {
                crate::navigate::Confidence::High => "",
                crate::navigate::Confidence::Low => " [low]",
            };
            out.push_str(&format!(
                "  d{} {}:{}:{} {} via {} at {}:{} ({}){}\n",
                item.depth,
                item.display_path,
                item.line,
                item.column,
                item.symbol,
                item.via_symbol,
                item.reference_line,
                item.reference_column,
                item.reference_kind,
                confidence
            ));
        }
        if self.truncated {
            out.push_str(&format!(
                "(showing {} of {})\n",
                self.impacted.len(),
                self.total
            ));
        }
        out
    }
}

impl ToolText for crate::navigate::ReadSymbolResponse {
    fn tool_text(&self) -> String {
        if let Some(body) = &self.body {
            let label = body.signature.as_deref().unwrap_or(&self.symbol);
            let mut out = format!(
                "{}:{}-{} {} {}\n",
                body.display_path, body.start_line, body.end_line, body.kind, label
            );
            out.push_str("```text\n");
            out.push_str(&body.content);
            if !body.content.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n");
            return out;
        }
        if self.matches.is_empty() {
            return format!("read_symbol: no matches for {}\n", self.symbol);
        }
        let mut out = if self.total == 1 {
            format!(
                "read_symbol: 1 match for {} (body omitted; set include_body=true to read it)\n",
                self.symbol
            )
        } else {
            format!(
                "read_symbol: {} matches for {} (ambiguous; refine with path, language, or kind)\n",
                self.total, self.symbol
            )
        };
        for item in &self.matches {
            match &item.signature {
                Some(signature) => out.push_str(&format!(
                    "  {}:{}:{} {} {}\n",
                    item.display_path, item.line, item.column, item.kind, signature
                )),
                None => out.push_str(&format!(
                    "  {}:{}:{} {}\n",
                    item.display_path, item.line, item.column, item.kind
                )),
            }
        }
        if self.truncated {
            out.push_str(&format!(
                "(showing {} of {})\n",
                self.matches.len(),
                self.total
            ));
        }
        out
    }
}

impl ToolText for crate::navigate::DefinitionResponse {
    fn tool_text(&self) -> String {
        if self.definitions.is_empty() {
            return format!("(no definitions for {})\n", self.symbol);
        }
        let mut out = String::new();
        for def in &self.definitions {
            match &def.signature {
                Some(sig) => out.push_str(&format!(
                    "{}:{}:{} {} {}\n",
                    def.display_path, def.line, def.column, def.kind, sig
                )),
                None => out.push_str(&format!(
                    "{}:{}:{} {}\n",
                    def.display_path, def.line, def.column, def.kind
                )),
            }
        }
        if self.truncated {
            out.push_str(&format!(
                "(showing {} of {})\n",
                self.definitions.len(),
                self.total
            ));
        }
        out
    }
}

impl ToolText for crate::navigate::ReferencesResponse {
    fn tool_text(&self) -> String {
        let mut out = String::new();
        if !self.definitions.is_empty() {
            out.push_str("definitions:\n");
            for def in &self.definitions {
                out.push_str(&format!(
                    "  {}:{}:{} {}\n",
                    def.display_path, def.line, def.column, def.kind
                ));
            }
        }
        if self.references.is_empty() {
            out.push_str(&format!("(no references to {})\n", self.symbol));
            return out;
        }
        if self.definition_count > 1 {
            out.push_str(&format!(
                "references ({} definitions of this name \u{2014} low-confidence may be unrelated):\n",
                self.definition_count
            ));
        } else {
            out.push_str("references:\n");
        }
        for r in &self.references {
            let mark = match r.confidence {
                crate::navigate::Confidence::High => "",
                crate::navigate::Confidence::Low => "  [low]",
            };
            out.push_str(&format!(
                "  {}:{}:{} {}{}\n",
                r.display_path, r.line, r.column, r.kind, mark
            ));
        }
        if self.truncated {
            out.push_str(&format!(
                "(showing {} of {})\n",
                self.references.len(),
                self.total
            ));
        }
        out
    }
}

impl ToolText for crate::navigate::CallHierarchyResponse {
    fn tool_text(&self) -> String {
        let mut out = String::new();
        let render = |label: &str, edges: &[crate::navigate::CallEdge], out: &mut String| {
            if edges.is_empty() {
                return;
            }
            out.push_str(label);
            out.push('\n');
            for e in edges {
                match &e.text {
                    Some(t) => out.push_str(&format!(
                        "  {}:{}:{} {} {}\n",
                        e.display_path, e.line, e.column, e.symbol, t
                    )),
                    None => out.push_str(&format!(
                        "  {}:{}:{} {}\n",
                        e.display_path, e.line, e.column, e.symbol
                    )),
                }
            }
        };
        render("callers (incoming):", &self.incoming, &mut out);
        render("callees (outgoing):", &self.outgoing, &mut out);
        if out.is_empty() {
            out.push_str(&format!("(no call hierarchy for {})\n", self.symbol));
        }
        out
    }
}
