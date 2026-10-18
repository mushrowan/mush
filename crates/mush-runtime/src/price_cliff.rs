use mush_ai::types::{Message, Model, Timestamp, Usage, UserContent, UserMessage};

pub const PRICE_CLIFF_TAG: &str = "[price cliff]";

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PriceCliff {
    pub model_id: String,
    pub over_prompt_tokens: u64,
    pub prompt_tokens: u64,
    pub input: f64,
    pub output: f64,
    pub cliff_input: f64,
    pub cliff_output: f64,
}

impl PriceCliff {
    #[must_use]
    pub fn crossed(model: &Model, usage: &Usage) -> Option<Self> {
        let prompt = mush_ai::models::prompt_tokens(usage);
        let tier = model.cost.tier_for(prompt)?;
        Some(Self {
            model_id: model.id.to_string(),
            over_prompt_tokens: tier.over_prompt_tokens,
            prompt_tokens: prompt.get(),
            input: model.cost.input,
            output: model.cost.output,
            cliff_input: tier.input,
            cliff_output: tier.output,
        })
    }

    #[must_use]
    pub fn notice(&self) -> String {
        let times = if self.input > 0.0 {
            format!("{}x", trim(self.cliff_input / self.input))
        } else {
            "more".to_string()
        };
        format!(
            "{PRICE_CLIFF_TAG}\n\nThe last request to {model} sent a {prompt} token prompt, past its \
             {over} token price tier. Every request over it costs {times}: ${cliff_input} and \
             ${cliff_output} per million input and output tokens instead of ${input} and ${output}. \
             Compact, or hand the rest of the work to a fresh child, to get back under.",
            model = self.model_id,
            prompt = thousands(self.prompt_tokens),
            over = thousands(self.over_prompt_tokens),
            cliff_input = trim(self.cliff_input),
            cliff_output = trim(self.cliff_output),
            input = trim(self.input),
            output = trim(self.output),
        )
    }

    #[must_use]
    pub fn message(&self) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(self.notice()))
                .timestamp_ms(Timestamp::now())
                .build(),
        )
    }
}

fn trim(value: f64) -> String {
    let text = format!("{value:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::TokenCount;

    fn haiku() -> Model {
        mush_ai::models::anthropic_models()
            .into_iter()
            .find(|model| model.id.as_str() == "claude-haiku-5-5")
            .unwrap()
    }

    fn usage(input: u64, cache_read: u64) -> Usage {
        Usage {
            input_tokens: TokenCount::new(input),
            output_tokens: TokenCount::new(500),
            cache_read_tokens: TokenCount::new(cache_read),
            cache_write_tokens: TokenCount::ZERO,
            ..Default::default()
        }
    }

    #[test]
    fn a_prompt_past_the_tier_crosses_the_cliff_and_says_what_it_costs() {
        assert_eq!(PriceCliff::crossed(&haiku(), &usage(40_000, 60_000)), None);

        let cliff = PriceCliff::crossed(&haiku(), &usage(4_312, 100_000)).unwrap();
        assert_eq!(cliff.over_prompt_tokens, 100_000);
        assert_eq!(cliff.prompt_tokens, 104_312);
        let notice = cliff.notice();
        assert!(notice.starts_with("[price cliff]\n\n"), "{notice}");
        assert!(
            notice.contains("a 104,312 token prompt, past its 100,000 token price tier"),
            "{notice}"
        );
        assert!(notice.contains("costs 5x: $0.5 and $2.5"), "{notice}");
        assert!(notice.contains("instead of $0.1 and $0.5"), "{notice}");
    }

    #[test]
    fn a_model_without_tiers_never_crosses() {
        let mut flat = haiku();
        flat.cost.tiers.clear();
        assert_eq!(PriceCliff::crossed(&flat, &usage(900_000, 0)), None);
    }
}
