//! Explicit-only windowless native spelling effect probe. No native params or
//! dictionary mutations are fabricated. No Win32 input/default GPUI presenter.
use rust_cef_core::{
    contracts::ThreadMode,
    menu::{ContextMenuResponse, MenuLimits},
    menu_context::MenuContextLimits,
    menu_lease::MenuLeaseOwner,
    ports::{
        BrowserCommand, BrowserHandle, ContextMenuTarget, MouseClick, MouseModifiers, WindowBinding,
    },
};
use rust_cef_runtime::{
    ContextMenuConfig, ContextMenuRegistration, SpellingConfig,
    menu_bridge_fixture::MenuBridgeFixture, runtime::Runtime,
    test_support::SpellingProbeReport as Report,
};
use std::{
    num::NonZeroUsize,
    time::{Duration, Instant},
};
type Tick = fn(&mut Runtime, ThreadMode) -> Result<(), String>;
pub struct SpellingProbe {
    owner: Option<MenuLeaseOwner>,
    binding: Option<ContextMenuRegistration>,
    report: Report,
}
fn hash(text: &str) -> u32 {
    text.as_bytes().iter().fold(2166136261u32, |h, b| {
        (h ^ u32::from(*b)).wrapping_mul(16777619)
    })
}
impl SpellingProbe {
    pub fn new() -> Self {
        Self {
            owner: None,
            binding: None,
            report: Report {
                metadata_owner_only: true,
                ..Default::default()
            },
        }
    }
    pub fn run(
        &mut self,
        rt: &mut Runtime,
        mode: ThreadMode,
        handle: &BrowserHandle,
        tick: Tick,
    ) -> Result<(), String> {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        let browser = handle.snapshot().browser;
        rt.configure_context_menus(ContextMenuConfig {
            capacity: nz(1),
            model_limits: MenuLimits {
                max_entries: nz(32),
                max_label_bytes: nz(256),
                max_total_label_bytes: nz(2048),
            },
            context_limits: MenuContextLimits {
                max_frame_id_bytes: nz(512),
                max_text_bytes: nz(4096),
                max_suggestions: nz(5),
                max_total_bytes: nz(8192),
            },
        })
        .map_err(|e| format!("spelling menu config: {e:?}"))?;
        let owner = MenuLeaseOwner::new(
            WindowBinding {
                browser,
                window: browser.local(),
            },
            nz(1),
        )
        .map_err(|e| format!("spelling owner: {e:?}"))?;
        let inbox = MenuBridgeFixture::new(owner.lease());
        let binding = rt
            .bind_context_menu(ContextMenuTarget {
                lease: owner.lease(),
                delivery: inbox.clone(),
            })
            .map_err(|e| format!("spelling bind: {e:?}"))?;
        binding
            .configure_spelling(SpellingConfig {
                max_word_bytes: nz(128),
                allow_dictionary_write: false,
            })
            .map_err(|e| format!("spelling capability: {e:?}"))?;
        self.binding = Some(binding);
        self.owner = Some(owner);
        self.report.phase = 1;
        let deadline = Instant::now() + Duration::from_secs(20);
        while handle.snapshot().navigation.title != "spelling-await-activation" {
            if Instant::now() >= deadline {
                return Err("spelling controlled editor readiness timeout".into());
            }
            tick(rt, mode)?;
        }
        handle
            .submit(BrowserCommand::MouseClick(MouseClick {
                x: 20,
                y: 20,
                modifiers: MouseModifiers::default(),
                click_count: std::num::NonZeroU8::new(1).unwrap(),
            }))
            .map_err(|e| format!("spelling activation click admission: {e:?}"))?;
        self.report.activation_input_submitted = true;
        while handle.snapshot().navigation.title != "spelling-ready:1:1" {
            if handle.snapshot().navigation.title == "spelling-activation-rejected" {
                return Err("spelling trusted activation rejected by original page".into());
            }
            if Instant::now() >= deadline {
                return Err("spelling trusted editor readiness timeout".into());
            }
            tick(rt, mode)?;
        }
        let actual_ready = handle.snapshot().navigation.title;
        let mut ready_fields = actual_ready.split(':');
        if ready_fields.next() != Some("spelling-ready") {
            return Err("invalid activation witness".into());
        }
        let trusted = ready_fields.next().ok_or("missing trusted event witness")? == "1";
        let active = ready_fields
            .next()
            .ok_or("missing active-at-edit witness")?
            == "1";
        if ready_fields.next().is_some() || !trusted || !active {
            return Err("spelling edit without trusted activation".into());
        }
        self.report.activation_witness = Some([trusted, active]);
        rt.probe_menu_spelling_open(browser, false)
            .map_err(|e| format!("spelling original CEF input: {e:?}"))?;
        let delivery = loop {
            if let Some(e) = self.binding.as_ref().unwrap().status().first_failure {
                return Err(format!("spelling original native context: {e}"));
            }
            if let Some(d) = inbox.take() {
                break d;
            }
            if Instant::now() >= deadline {
                return Err("spelling original menu delivery timeout".into());
            }
            tick(rt, mode)?;
        };
        let context = &delivery.presentation.context;
        self.report.word_bytes = context.misspelled_word.len();
        self.report.selection_bytes = context.selection.len();
        self.report.suggestion_count = context.dictionary_suggestions.len();
        self.report.frame_id_bytes = context.frame_id.len();
        if context.misspelled_word != "mispeling" || context.selection != context.misspelled_word {
            return Err("native misspelled original word/selection absent or changed".into());
        }
        let first = context
            .dictionary_suggestions
            .first()
            .filter(|s| !s.is_empty())
            .ok_or("native dictionary supplied no original suggestion")?;
        self.report.replacement_bytes = first.len();
        self.report.expected_hash = hash(first);
        while rt.diagnostics().tasks.pending != 0 || rt.diagnostics().pending_requests != 1 {
            if Instant::now() >= deadline {
                return Err("spelling original callback return timeout".into());
            }
            tick(rt, mode)?;
        }
        delivery
            .response
            .submit(
                delivery.presentation.request,
                ContextMenuResponse::Select { command_id: 200 },
            )
            .map_err(|e| format!("spelling exact response: {e:?}"))?;
        let expected = format!(
            "spelling-effect:{}:2:insertReplacementText",
            self.report.expected_hash
        );
        let (observed_hash, observed_input_count) = loop {
            let s = self.binding.as_ref().unwrap().spelling_status();
            if let Some(e) = s.first_failure {
                return Err(format!("spelling native issue: {e}"));
            }
            let actual_title = handle.snapshot().navigation.title;
            if s.replacement_issued == 1
                && actual_title == expected
                && inbox.snapshot().terminal == 1
                && rt.diagnostics().tasks.pending == 0
            {
                let mut fields = actual_title.split(':');
                if fields.next() != Some("spelling-effect") {
                    return Err("invalid spelling effect witness prefix".into());
                }
                let observed_hash = fields
                    .next()
                    .and_then(|v| v.parse::<u32>().ok())
                    .ok_or("invalid observed spelling hash")?;
                let count = fields
                    .next()
                    .and_then(|v| v.parse::<u32>().ok())
                    .ok_or("invalid observed input count")?;
                if fields.next() != Some("insertReplacementText") || fields.next().is_some() {
                    return Err("invalid spelling input event witness".into());
                }
                break (observed_hash, count);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "spelling actual replacement/input event timeout (issued={}, title_matches={})",
                    s.replacement_issued,
                    handle.snapshot().navigation.title == expected
                ));
            }
            tick(rt, mode)?;
        };
        self.report.effect_observed = true;
        self.report.observed_hash = observed_hash;
        self.report.original_input_event_count = observed_input_count;
        self.report.menu_terminal_separate = !self
            .binding
            .as_ref()
            .unwrap()
            .spelling_status()
            .effect_verified;
        self.report.phase = 2;
        rt.probe_menu_spelling_open(browser, true)
            .map_err(|e| format!("spelling disabled dictionary input: {e:?}"))?;
        loop {
            let s = self.binding.as_ref().unwrap().status();
            if let Some(e) = s.first_failure {
                if e != "missing required host capability for menu command 206" {
                    return Err(format!("spelling unexpected dictionary diagnostic: {e}"));
                }
                if rt.diagnostics().tasks.pending == 0 && rt.diagnostics().pending_requests == 0 {
                    self.report.dictionary_disabled_rejected = true;
                    break;
                }
            }
            if Instant::now() >= deadline {
                return Err("spelling missing dictionary capability cancellation timeout".into());
            }
            tick(rt, mode)?;
        }
        self.report.fixture = inbox.snapshot();
        self.report.phase = 3;
        Ok(())
    }
    pub fn snapshot(&self) -> Report {
        let mut r = self.report.clone();
        if let Some(b) = &self.binding {
            r.native_context = b.probe_spelling_context();
            r.status = b.spelling_status();
            r.menu_retired = b.status().retired;
            r.menu_failure = b.status().first_failure;
        }
        r
    }
}
pub fn fixture_url() -> String {
    let html = r#"<body style='margin:0;font:24px Arial;background:rgb(26,76,126)'><div id='e' contenteditable='true' spellcheck='true' lang='en-US' style='min-height:40px'></div><script>const e=document.getElementById('e');let changes=0;e.addEventListener('input',(event)=>{changes++;let h=2166136261;for(const b of new TextEncoder().encode(e.textContent.trim()))h=Math.imul(h^b,16777619)>>>0;document.title='spelling-effect:'+h+':'+changes+':'+event.inputType;});document.title='spelling-await-activation';e.addEventListener('click',(event)=>{const trusted=event.isTrusted;const active=navigator.userActivation.isActive;if(!trusted||!active){document.title='spelling-activation-rejected';return;}document.execCommand('insertText',false,'mispeling ');setTimeout(()=>{const r=document.createRange();r.setStart(e.firstChild,0);r.setEnd(e.firstChild,9);window.getSelection().removeAllRanges();window.getSelection().addRange(r);document.title='spelling-ready:'+Number(trusted)+':'+Number(active);},2000);},{once:true});</script></body>"#;
    let mut url = String::from("data:text/html,");
    for b in html.bytes() {
        if b.is_ascii_alphanumeric() {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    url
}
