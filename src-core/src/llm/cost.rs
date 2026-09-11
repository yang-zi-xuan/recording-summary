//! 费用估算。见技术方案 §10.7。
//!
//! 价格**不写死** —— 做成可配置的参考价,并明确标注是估算。
//! 用户选本地转写就是为了省钱,应该让他看得见省了多少。

use crate::types::TokenUsage;
use serde::{Deserialize, Serialize};

/// 单位:元 / 百万 token。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelPrice {
    pub input_per_million: f64,
    pub output_per_million: f64,
    /// 缓存命中的输入价(DeepSeek 有折扣,这是"把稳定内容前置"的收益来源)
    pub cached_input_per_million: f64,
}

impl Default for ModelPrice {
    fn default() -> Self {
        Self {
            input_per_million: 2.0,
            output_per_million: 8.0,
            cached_input_per_million: 0.5,
        }
    }
}

/// 价格表。可按模型配置;未命中时用默认值。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PriceTable {
    pub currency: String,
    pub default: ModelPrice,
    /// 模型名 → 价格
    #[serde(default)]
    pub per_model: std::collections::BTreeMap<String, ModelPrice>,
}

impl PriceTable {
    pub fn cny() -> Self {
        Self {
            currency: "CNY".into(),
            default: ModelPrice::default(),
            per_model: Default::default(),
        }
    }

    pub fn price_for(&self, model: &str) -> &ModelPrice {
        self.per_model.get(model).unwrap_or(&self.default)
    }

    /// 估算费用。
    ///
    /// 注意 `usage.input` 通常**已包含**缓存命中的部分,所以要把命中部分
    /// 拆出来按折扣价算,否则会高估。
    pub fn estimate(&self, model: &str, usage: &TokenUsage) -> f64 {
        let p = self.price_for(model);
        let cached = usage.cached_input.min(usage.input);
        let fresh = usage.input.saturating_sub(cached);

        let cost = fresh as f64 / 1_000_000.0 * p.input_per_million
            + cached as f64 / 1_000_000.0 * p.cached_input_per_million
            + usage.output as f64 / 1_000_000.0 * p.output_per_million;
        cost
    }

    /// 人类可读的费用字符串。金额很小时不显示 "0.00" 而显示更多小数位。
    pub fn format_cost(&self, model: &str, usage: &TokenUsage) -> String {
        let c = self.estimate(model, usage);
        let unit = if self.currency == "CNY" { "元" } else { &self.currency };
        if c <= 0.0 {
            format!("≈0 {unit}")
        } else if c < 0.01 {
            format!("≈{c:.4} {unit}")
        } else if c < 1.0 {
            format!("≈{c:.3} {unit}")
        } else {
            format!("≈{c:.2} {unit}")
        }
    }

    /// 缓存命中省下的钱 —— 用来验证"稳定内容前置"这个 prompt 结构确实有效。
    pub fn cache_savings(&self, model: &str, usage: &TokenUsage) -> f64 {
        let p = self.price_for(model);
        let cached = usage.cached_input.min(usage.input);
        cached as f64 / 1_000_000.0 * (p.input_per_million - p.cached_input_per_million)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_splits_cached_tokens() {
        let t = PriceTable::cny();
        // 1M 输入全命中缓存 + 1M 输出
        let u = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cached_input: 1_000_000,
        };
        let c = t.estimate("deepseek-chat", &u);
        // 0.5(缓存输入) + 8.0(输出)
        assert!((c - 8.5).abs() < 1e-6, "得到 {c}");
    }

    #[test]
    fn estimate_without_cache_uses_full_input_price() {
        let t = PriceTable::cny();
        let u = TokenUsage {
            input: 1_000_000,
            output: 0,
            cached_input: 0,
        };
        assert!((t.estimate("m", &u) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn cached_cannot_exceed_input() {
        let t = PriceTable::cny();
        let u = TokenUsage {
            input: 100,
            output: 0,
            cached_input: 999_999, // 异常数据
        };
        // 不应产生负成本
        assert!(t.estimate("m", &u) >= 0.0);
    }

    #[test]
    fn format_cost_handles_tiny_amounts() {
        let t = PriceTable::cny();
        let u = TokenUsage {
            input: 15_000,
            output: 3_000,
            cached_input: 12_000,
        };
        let s = t.format_cost("deepseek-chat", &u);
        assert!(s.contains("元"), "{s}");
        // 一节 45 分钟课的总结成本应是"几厘钱"量级,不能显示成 0.00
        assert!(!s.contains("≈0.00 "), "不应把小额显示成 0.00: {s}");
    }

    #[test]
    fn zero_usage_formats_as_zero() {
        let t = PriceTable::cny();
        assert!(t.format_cost("m", &TokenUsage::default()).contains('0'));
    }

    #[test]
    fn cache_savings_positive_when_cache_hits() {
        let t = PriceTable::cny();
        let u = TokenUsage {
            input: 1_000_000,
            output: 0,
            cached_input: 1_000_000,
        };
        // 2.0 - 0.5 = 1.5
        assert!((t.cache_savings("m", &u) - 1.5).abs() < 1e-6);
    }

    #[test]
    fn per_model_override_is_used() {
        let mut t = PriceTable::cny();
        t.per_model.insert(
            "cheap".into(),
            ModelPrice {
                input_per_million: 0.0,
                output_per_million: 0.0,
                cached_input_per_million: 0.0,
            },
        );
        let u = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cached_input: 0,
        };
        assert_eq!(t.estimate("cheap", &u), 0.0);
        assert!(t.estimate("other", &u) > 0.0);
    }
}
