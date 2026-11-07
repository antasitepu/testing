use analysis_domain::{ManagementDirective, SignalDefinition, SignalEvent};
use market_domain::{AggregateTrade, Candle, MarketSession, Symbol};
use rust_decimal::Decimal;

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum TrackerInput {
    RecommendationOpened(SignalDefinition),
    SignalEvent(SignalEvent),
    Directive(ManagementDirective),
    ReverseAtNextOpen {
        directive: ManagementDirective,
        reverse_signal: SignalDefinition,
        canonical_open: Candle,
    },
    AggregateTrade(AggregateTrade),
    ReferencePrice {
        symbol: Symbol,
        time: i64,
        price: Decimal,
    },
    OhlcReconciliation(Candle),
    /// A stored M1 candle for trades still catching up from history.
    ReplayCandle(Candle),
    CanonicalOpen(Candle),
    SessionChanged(MarketSession),
}
