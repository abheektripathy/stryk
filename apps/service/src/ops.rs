use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[derive(Default)]
pub struct Operations {
    pub rpc_rate_limited: AtomicU64,
    pub rfq_rejected: AtomicU64,
    pub rfq_quoted: AtomicU64,
    pub rfq_no_quote: AtomicU64,
    pub finalized_slot: AtomicU64,
    pub latest_signature_slot: AtomicU64,
    pub receipt_scan_slot: AtomicU64,
    pub receipt_scan_healthy: AtomicU64,
    pub oldest_unsettled_seconds: AtomicU64,
    pub pending_fixes: AtomicU64,
    pub no_fix_expiries: AtomicU64,
    pub next_fix_deadline_seconds: AtomicU64,
    pub settlement_scan_healthy: AtomicU64,
    pub settlement_failed: AtomicU64,
}

impl Operations {
    pub fn render(&self, makers: usize) -> String {
        let finalized = self.finalized_slot.load(Relaxed);
        let caught_up = self.receipt_scan_slot.load(Relaxed);
        let signature = self.latest_signature_slot.load(Relaxed);
        format!(
            concat!(
                "# TYPE stryk_rpc_rate_limited_total counter\nstryk_rpc_rate_limited_total {}\n",
                "# TYPE stryk_rfq_rejected_capacity_total counter\nstryk_rfq_rejected_capacity_total {}\n",
                "# TYPE stryk_rfq_quoted_total counter\nstryk_rfq_quoted_total {}\n",
                "# TYPE stryk_rfq_no_quote_total counter\nstryk_rfq_no_quote_total {}\n",
                "# TYPE stryk_maker_subscriptions gauge\nstryk_maker_subscriptions {}\n",
                "# TYPE stryk_receipt_scan_healthy gauge\nstryk_receipt_scan_healthy {}\n",
                "# TYPE stryk_finalized_slot gauge\nstryk_finalized_slot {}\n",
                "# TYPE stryk_receipt_scan_slot gauge\nstryk_receipt_scan_slot {}\n",
                "# TYPE stryk_latest_program_signature_slot gauge\nstryk_latest_program_signature_slot {}\n",
                "# TYPE stryk_receipt_scan_slot_lag gauge\nstryk_receipt_scan_slot_lag {}\n",
                "# TYPE stryk_oldest_unsettled_seconds gauge\nstryk_oldest_unsettled_seconds {}\n",
                "# TYPE stryk_pending_fixes gauge\nstryk_pending_fixes {}\n",
                "# TYPE stryk_no_fix_expiries gauge\nstryk_no_fix_expiries {}\n",
                "# TYPE stryk_next_fix_deadline_seconds gauge\nstryk_next_fix_deadline_seconds {}\n",
                "# TYPE stryk_settlement_scan_healthy gauge\nstryk_settlement_scan_healthy {}\n",
                "# TYPE stryk_settlement_failed_total counter\nstryk_settlement_failed_total {}\n",
            ),
            self.rpc_rate_limited.load(Relaxed),
            self.rfq_rejected.load(Relaxed),
            self.rfq_quoted.load(Relaxed),
            self.rfq_no_quote.load(Relaxed),
            makers,
            self.receipt_scan_healthy.load(Relaxed),
            finalized,
            caught_up,
            signature,
            signature.saturating_sub(caught_up),
            self.oldest_unsettled_seconds.load(Relaxed),
            self.pending_fixes.load(Relaxed),
            self.no_fix_expiries.load(Relaxed),
            self.next_fix_deadline_seconds.load(Relaxed),
            self.settlement_scan_healthy.load(Relaxed),
            self.settlement_failed.load(Relaxed),
        )
    }
}
