//! Coalesced damage scope. Output identity prevents a stale request from
//! dirtying a reconnected output that happens to reuse the same name.

use smithay::output::Output;

#[derive(Default)]
pub(crate) struct RedrawRequests {
    all: bool,
    outputs: Vec<Output>,
}

impl RedrawRequests {
    pub(crate) fn global() -> Self {
        Self {
            all: true,
            outputs: Vec::new(),
        }
    }

    pub(crate) fn request_all(&mut self) {
        self.all = true;
        self.outputs.clear();
    }

    pub(crate) fn request_output(&mut self, output: &Output) {
        if !self.all && !self.outputs.contains(output) {
            self.outputs.push(output.clone());
        }
    }

    pub(crate) fn includes(&self, output: &Output) -> bool {
        self.all || self.outputs.contains(output)
    }

    pub(crate) fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::output::{PhysicalProperties, Subpixel};

    fn output(name: &str) -> Output {
        Output::new(
            name.into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
                serial_number: "test".into(),
            },
        )
    }

    #[test]
    fn output_requests_coalesce_without_dirtying_other_outputs() {
        let first = output("first");
        let second = output("second");
        let mut requests = RedrawRequests::default();
        for _ in 0..64 {
            requests.request_output(&first);
        }
        assert_eq!(requests.outputs.len(), 1);
        let batch = requests.take();
        assert!(batch.includes(&first));
        assert!(!batch.includes(&second));
        assert!(!requests.includes(&first));
        requests.request_output(&second);
        assert!(!requests.includes(&first));
        assert!(requests.includes(&second));
    }

    #[test]
    fn global_damage_supersedes_scoped_requests_in_any_order() {
        let first = output("first");
        let second = output("second");
        let mut requests = RedrawRequests::default();
        requests.request_output(&first);
        requests.request_all();
        requests.request_output(&second);
        assert!(requests.outputs.is_empty());
        let batch = requests.take();
        assert!(batch.includes(&first) && batch.includes(&second));
        assert!(!requests.includes(&first) && !requests.includes(&second));
    }

    #[test]
    fn reconnected_output_with_the_same_name_does_not_inherit_old_damage() {
        let old = output("DP-test");
        let replacement = output("DP-test");
        let mut requests = RedrawRequests::default();
        requests.request_output(&old);
        assert!(requests.includes(&old.clone()));
        assert!(!requests.includes(&replacement));
        assert!(RedrawRequests::global().includes(&replacement));
    }
}
