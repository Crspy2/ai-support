use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use ai_support::extensions::traits::{
    ActionDescriptor, ExtensionRegistry, ExtensionTrait, FetchDescriptor, HookDescriptor,
    HookEvent, HookHandlerFn,
};

// --- FlagExt: fires its hook only for the registered event ---

struct FlagExt {
    event: HookEvent,
    flag: Arc<AtomicBool>,
}

impl ExtensionTrait for FlagExt {
    fn name(&self) -> &'static str {
        "FlagExt"
    }

    fn fetchers(self: Arc<Self>) -> Vec<FetchDescriptor> {
        vec![]
    }

    fn actions(self: Arc<Self>) -> Vec<ActionDescriptor> {
        vec![]
    }

    fn hooks(self: Arc<Self>) -> Vec<HookDescriptor> {
        let event = self.event;
        let flag = Arc::clone(&self.flag);
        let handler: HookHandlerFn = Box::new(move |_| {
            let flag = Arc::clone(&flag);
            Box::pin(async move {
                flag.store(true, Ordering::SeqCst);
                Ok(())
            })
        });
        vec![HookDescriptor { event, handler }]
    }
}

// --- TwoHookExt: first hook fails, second sets a flag ---

struct TwoHookExt {
    flag: Arc<AtomicBool>,
}

impl ExtensionTrait for TwoHookExt {
    fn name(&self) -> &'static str {
        "TwoHookExt"
    }

    fn fetchers(self: Arc<Self>) -> Vec<FetchDescriptor> {
        vec![]
    }

    fn actions(self: Arc<Self>) -> Vec<ActionDescriptor> {
        vec![]
    }

    fn hooks(self: Arc<Self>) -> Vec<HookDescriptor> {
        let flag = Arc::clone(&self.flag);
        let fail: HookHandlerFn =
            Box::new(|_| Box::pin(async { anyhow::bail!("intentional failure") }));
        let success: HookHandlerFn = Box::new(move |_| {
            let flag = Arc::clone(&flag);
            Box::pin(async move {
                flag.store(true, Ordering::SeqCst);
                Ok(())
            })
        });
        vec![
            HookDescriptor { event: HookEvent::IssueProposed, handler: fail },
            HookDescriptor { event: HookEvent::IssueProposed, handler: success },
        ]
    }
}

// --- Tests ---

#[tokio::test]
async fn fire_hook_dispatches_to_matching_handler() {
    let fired = Arc::new(AtomicBool::new(false));
    let ext: Arc<dyn ExtensionTrait> = Arc::new(FlagExt {
        event: HookEvent::IssueProposed,
        flag: Arc::clone(&fired),
    });
    let registry = ExtensionRegistry::new(vec![ext]);
    registry.fire_hook(HookEvent::IssueProposed, serde_json::json!(null)).await;
    assert!(fired.load(Ordering::SeqCst));
}

#[tokio::test]
async fn fire_hook_skips_non_matching_event() {
    let fired = Arc::new(AtomicBool::new(false));
    // Handler is registered for IssueProposed …
    let ext: Arc<dyn ExtensionTrait> = Arc::new(FlagExt {
        event: HookEvent::IssueProposed,
        flag: Arc::clone(&fired),
    });
    let registry = ExtensionRegistry::new(vec![ext]);
    // … but we fire IssueAccepted — handler must not run.
    registry.fire_hook(HookEvent::IssueAccepted, serde_json::json!(null)).await;
    assert!(!fired.load(Ordering::SeqCst));
}

#[tokio::test]
async fn fire_hook_continues_after_failing_hook() {
    let flag = Arc::new(AtomicBool::new(false));
    let ext: Arc<dyn ExtensionTrait> = Arc::new(TwoHookExt { flag: Arc::clone(&flag) });
    let registry = ExtensionRegistry::new(vec![ext]);
    registry.fire_hook(HookEvent::IssueProposed, serde_json::json!(null)).await;
    assert!(flag.load(Ordering::SeqCst));
}
