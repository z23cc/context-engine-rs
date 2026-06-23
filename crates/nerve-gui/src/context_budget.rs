//! Context token budget cockpit: pressure badges and token breakdown bars.

use leptos::prelude::*;

const SOFT_BUDGET_TOKENS: usize = 128_000;
const HARD_BUDGET_TOKENS: usize = 200_000;

/// The token breakdown returned under `workspace_context` `structuredContent.tokens`.
#[derive(Clone, Default, serde::Deserialize)]
pub(crate) struct Budget {
    pub(crate) total_tokens: usize,
    pub(crate) file_map_tokens: usize,
    pub(crate) contents_tokens: usize,
    pub(crate) git_diff_tokens: usize,
    pub(crate) meta_prompts_tokens: usize,
    pub(crate) instructions_tokens: usize,
}

#[component]
pub(crate) fn BudgetPanel(budget: Budget) -> impl IntoView {
    let total = budget.total_tokens;
    view! {
        <div class="ctx-budget">
            <div class="ctx-budget-head">
                <div class="ctx-total">{total}" tokens"</div>
                <span class=format!("ctx-budget-badge {}", budget_class(total))>{budget_label(total)}</span>
            </div>
            <div class="ctx-budget-detail">{budget_detail(total)}</div>
            <div class="ctx-bars">
                <Bar label="files" n=budget.contents_tokens total=total/>
                <Bar label="map" n=budget.file_map_tokens total=total/>
                <Bar label="diff" n=budget.git_diff_tokens total=total/>
                <Bar label="meta" n=budget.meta_prompts_tokens total=total/>
                <Bar label="instr" n=budget.instructions_tokens total=total/>
            </div>
        </div>
    }
}

fn budget_class(total: usize) -> &'static str {
    if total >= HARD_BUDGET_TOKENS {
        "over"
    } else if total >= SOFT_BUDGET_TOKENS {
        "warn"
    } else {
        "ok"
    }
}

fn budget_label(total: usize) -> &'static str {
    if total == 0 {
        "empty"
    } else if total >= HARD_BUDGET_TOKENS {
        "over budget"
    } else if total >= SOFT_BUDGET_TOKENS {
        "watch"
    } else {
        "ready"
    }
}

fn budget_detail(total: usize) -> String {
    if total == 0 {
        return "Assemble context to see budget pressure.".into();
    }
    if total >= HARD_BUDGET_TOKENS {
        format!("Reduce selection: over the {HARD_BUDGET_TOKENS} token cockpit target.")
    } else if total >= SOFT_BUDGET_TOKENS {
        format!("Approaching the {HARD_BUDGET_TOKENS} token target; review selected files.")
    } else {
        format!("Within budget; warning starts at {SOFT_BUDGET_TOKENS} tokens.")
    }
}

#[component]
fn Bar(label: &'static str, n: usize, total: usize) -> impl IntoView {
    let pct = (n * 100).checked_div(total).unwrap_or(0);
    view! {
        <div class="ctx-bar-row">
            <span class="ctx-bar-label">{label}</span>
            <span class="ctx-bar-track"><span class="ctx-bar-fill" style=format!("width:{pct}%")></span></span>
            <span class="ctx-bar-n">{n}</span>
        </div>
    }
}
