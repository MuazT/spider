use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::clone;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use url::Url;
use webkit::prelude::*;
use webkit::{
    HardwareAccelerationPolicy, LoadEvent, NetworkProxyMode, NetworkProxySettings,
    PolicyDecisionType, WebContext, WebView,
};

use crate::apps::{
    self, get_app_details, get_app_permission, permission_label, set_app_permission, AppDetails,
};

/// Stock Linux Chromium agent. WebKitGTK's own default claims to be
/// Safari on macOS, which sites like WhatsApp Web reject ("browser not
/// supported") and gate calling behind.
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36";

fn format_css(id: &str, bg: &str, fg: &str) -> String {
    format!(
        r#"window#s{id} {{
    background: {bg};
    color: {fg};
}}"#
    )
}

// Source: https://www.w3.org/WAI/GL/wiki/Relative_luminance
fn luminence(rgba: gdk::RGBA) -> f32 {
    0.2126 * rgba.red() + 0.7152 * rgba.green() + 0.0722 * rgba.blue()
}

/// Checks whether navigating to `uri` is allowed by the app's domain
/// restriction. Subdomains of an allowed domain are allowed too. URIs
/// without a host (about:, blob:, data:, ...) are always allowed since
/// web apps commonly rely on them internally.
fn uri_allowed(allowed_domains: &Option<Vec<String>>, uri: &str) -> bool {
    let Some(allowed) = allowed_domains else {
        return true;
    };
    let Ok(url) = Url::parse(uri) else {
        return false;
    };
    match url.host_str() {
        None => true,
        Some(host) => {
            let host = host.to_lowercase();
            allowed
                .iter()
                .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
        }
    }
}

/// The scheme://host[:port] origin of a URI, used as the storage key for
/// website permissions.
fn page_origin(uri: Option<&str>) -> Option<String> {
    let url = Url::parse(uri?).ok()?;
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

/// Maps WebKit's permission API names (from PermissionStateQuery) onto our
/// stored kinds.
fn normalize_permission_name(name: Option<&str>) -> Option<&'static str> {
    let name = name?;
    if name.contains("audio") {
        Some("microphone")
    } else if name.contains("video") {
        Some("camera")
    } else {
        apps::PERMISSION_LABELS
            .iter()
            .find(|(kind, _)| *kind == name)
            .map(|(kind, _)| *kind)
    }
}

/// Which stored permission kinds a WebKit permission request maps to.
/// Multiple kinds are returned when e.g. camera AND microphone are both
/// requested; all of them share one prompt/decision.
fn permission_kinds(request: &webkit::PermissionRequest) -> Vec<&'static str> {
    if let Ok(media) = request
        .clone()
        .upcast::<glib::Object>()
        .downcast::<webkit::UserMediaPermissionRequest>()
    {
        let mut kinds = Vec::new();
        if media.is_for_audio_device() {
            kinds.push("microphone");
        }
        if media.is_for_video_device() {
            kinds.push("camera");
        }
        return kinds;
    }
    let type_name = request.type_().name();
    match type_name {
        "WebKitNotificationPermissionRequest" => vec!["notifications"],
        "WebKitGeolocationPermissionRequest" => vec!["geolocation"],
        _ => Vec::new(),
    }
}

mod imp {

    use super::*;

    #[derive(Default, Debug, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/zaedus/spider/app_window.ui")]
    pub struct AppWindow {
        #[template_child]
        pub toolbar: TemplateChild<adw::ToolbarView>,
        #[template_child]
        pub webview_container: TemplateChild<adw::Bin>,
        #[template_child]
        pub progress_bar: TemplateChild<gtk::ProgressBar>,
        #[template_child]
        pub back_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub forward_button: TemplateChild<gtk::Button>,

        pub details: RefCell<AppDetails>,
        pub webview: RefCell<webkit::WebView>,
        pub provider: RefCell<Option<gtk::CssProvider>>,
        // Web notifications awaiting a click on their desktop twin
        pub pending_notifications: RefCell<Vec<webkit::Notification>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AppWindow {
        const NAME: &'static str = "AppWindow";
        type Type = super::AppWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_callbacks();
        }
        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for AppWindow {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().setup_gestures();
            self.obj().setup_gactions();
        }
    }
    impl WidgetImpl for AppWindow {}
    impl WindowImpl for AppWindow {
        fn close_request(&self) -> glib::Propagation {
            let size = self.obj().default_size();
            if let Some(mut details) = get_app_details(&self.details.borrow().id) {
                details.window_width = size.0;
                details.window_height = size.1;
                details.window_maximize = self.obj().is_maximized();
                details.save().unwrap(); // App is closing, shouldn't fail really ever
            }

            // "Run in background": hide the window instead of closing so
            // the app keeps running (reopening it from the launcher
            // presents this same instance again)
            if self.details.borrow().run_in_background {
                self.obj().set_visible(false);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    }
    impl ApplicationWindowImpl for AppWindow {}
    impl AdwApplicationWindowImpl for AppWindow {}

    impl AppWindow {
        pub fn set_details(&self, details: &AppDetails) {
            self.details.replace(details.clone());

            // Configure window
            self.obj()
                .set_widget_name(format!("s{}", details.id).as_str());
            self.obj().set_title(Some(details.title.as_str()));
            self.obj().load_window_size();

            // Set up the WebView
            let webview = self.create_webview();
            webview.load_uri(&details.url);
            self.webview_container.set_child(Some(&webview));
            self.webview.replace(webview);

            self.load_colors(None);
        }

        fn load_colors(&self, bg: Option<&str>) {
            if self.provider.borrow().is_none() {
                let display = gdk::Display::default().unwrap();
                let provider = gtk::CssProvider::new();

                gtk::style_context_add_provider_for_display(
                    &display,
                    &provider,
                    gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
                );
                self.provider.replace(Some(provider));
            }
            // Add new one if possible
            let provider = self.provider.borrow();
            let provider = provider.as_ref().unwrap();
            let fg = bg.and_then(|bg| {
                gdk::RGBA::parse(bg)
                    .map(|rgba| {
                        if luminence(rgba) > 0.5 {
                            "black"
                        } else {
                            "white"
                        }
                    })
                    .ok()
            });
            provider.load_from_string(
                format_css(
                    self.details.borrow().id.as_str(),
                    bg.unwrap_or("@window_bg_color"),
                    fg.unwrap_or("@window_fg_color"),
                )
                .as_str(),
            );
        }
        fn create_webview(&self) -> webkit::WebView {
            let details = self.details.borrow();
            let id = details.id.as_str();

            // Define and create base app directory where webkit cache, data, and cookies are stored
            let app_data_dir = glib::user_data_dir()
                .join(glib::application_name().unwrap())
                .join(id);
            let app_cache_dir = glib::user_cache_dir()
                .join(glib::application_name().unwrap())
                .join(id);
            std::fs::create_dir_all(app_data_dir.clone()).unwrap();
            std::fs::create_dir_all(app_cache_dir.clone()).unwrap();

            // Build settings
            let mut settings = webkit::Settings::builder()
                .enable_webgl(true)
                .enable_webrtc(true)
                // Required for getUserMedia + RTCPeerConnection; sites like
                // WhatsApp Web report "calling not supported" without it
                .enable_media_stream(true)
                .enable_media(true)
                .enable_mediasource(true)
                .enable_encrypted_media(true)
                .enable_media_capabilities(true)
                .hardware_acceleration_policy(HardwareAccelerationPolicy::Always)
                .enable_2d_canvas_acceleration(true)
                .enable_html5_local_storage(true)
                .enable_html5_database(true)
                .enable_site_specific_quirks(true)
                .enable_developer_extras(true);
            let effective_user_agent = details
                .user_agent
                .clone()
                .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string());
            if let Some(user_agent) = &details.user_agent {
                settings = settings.user_agent(user_agent);
            } else {
                settings = settings.user_agent(DEFAULT_USER_AGENT);
            }
            let settings = settings.build();

            // Build network session
            let network_session = webkit::NetworkSession::builder()
                .cache_directory(app_cache_dir.to_str().unwrap())
                .data_directory(app_data_dir.join("data").to_str().unwrap())
                .build();

            // Apply custom HTTP proxy if one is configured
            if let Some(proxy_url) = &details.proxy_url {
                let proxy_settings = NetworkProxySettings::new(Some(proxy_url.as_str()), &[]);
                network_session.set_proxy_settings(NetworkProxyMode::Custom, Some(&proxy_settings));
            }

            network_session.connect_download_started(|_, dl| {
                dl.connect_decide_destination(move |dl, dest| {
                    let dest = dest.to_string();
                    glib::spawn_future_local(clone!(
                        #[weak]
                        dl,
                        async move {
                            let dialog = gtk::FileDialog::builder()
                                .accept_label("Save")
                                .title("Download file")
                                .modal(false)
                                .initial_name(dest.as_str())
                                .build();
                            if let Some(path) = dialog
                                .save_future(None::<&gtk::Window>)
                                .await
                                .ok()
                                .and_then(|f| f.path())
                            {
                                let path = path.as_os_str().to_str().unwrap();
                                dl.set_destination(path);
                            }
                        }
                    ));
                    true
                });
            });

            // Build cookie manager
            let cookie_manager = network_session.cookie_manager().unwrap();

            cookie_manager.set_persistent_storage(
                app_data_dir.join("cookie").to_str().unwrap(),
                webkit::CookiePersistentStorage::Sqlite,
            );

            // Build content manager
            let content_manager = webkit::UserContentManager::new();
            if details.has_titlebar_color {
                let script = webkit::UserScript::new(
                    include_str!("./inject.js"),
                    webkit::UserContentInjectedFrames::TopFrame,
                    webkit::UserScriptInjectionTime::End,
                    &[],
                    &[],
                );
                content_manager.register_script_message_handler("themeColor", None);
                content_manager.connect_script_message_received(
                    Some("themeColor"),
                    clone!(
                        #[weak(rename_to=_self)]
                        self,
                        move |_, value| {
                            let value = value.to_str();
                            if value != "null" {
                                _self.load_colors(Some(value.as_str()));
                            } else {
                                _self.load_colors(None)
                            }
                        }
                    ),
                );
                content_manager.add_script(&script);
            }

            // Shim navigator.userAgentData (Client Hints). WebKitGTK does not
            // implement it, but Chromium-only web apps (e.g. WhatsApp Web
            // calling) refuse to work when it is missing.
            let chrome_major = effective_user_agent
                .split("Chrome/")
                .nth(1)
                .and_then(|rest| rest.split('.').next())
                .unwrap_or("151")
                .to_string();
            let ua_ch_shim = format!(
                r#"(function() {{
    if (navigator.userAgentData) return;
    var v = '{v}';
    var brands = [
        {{ brand: 'Not_A Brand', version: '8' }},
        {{ brand: 'Chromium', version: v }},
        {{ brand: 'Google Chrome', version: v }}
    ];
    var data = {{
        brands: brands,
        mobile: false,
        platform: 'Linux',
        toJSON: function() {{
            return {{ brands: brands, mobile: false, platform: 'Linux' }};
        }},
        getHighEntropyValues: function(hints) {{
            return Promise.resolve({{
                architecture: 'x86',
                bitness: '64',
                model: '',
                platformVersion: '13.0.0',
                uaFullVersion: v + '.0.0.0',
                fullVersionList: brands,
                wow64: false
            }});
        }}
    }};
    Object.defineProperty(Navigator.prototype, 'userAgentData', {{
        get: function() {{ return data; }},
        configurable: true
    }});
}})();
"#,
                v = chrome_major
            );
            // Injected at document start; plain raw string (no format!), so
            // braces here are literal JS braces.
            let worker_shim = r#"
// WebKitGTK does not expose navigator.mediaDevices inside Web Workers, so
// Chromium-only apps (e.g. WhatsApp Web) that probe cameras from a worker
// conclude there is no camera. Two-part fix:
//  1. Prefix every JavaScript Blob with a bridge that defines
//     navigator.mediaDevices and forwards calls to the main thread over
//     postMessage (covers classic and module blob workers).
//  2. Wrap Worker() so same-origin URL workers get the same bridge and so
//     RPC replies are routed back into each worker instance.
(function() {
    if (typeof Worker === 'undefined') return;
    if (typeof navigator === 'undefined' || !navigator.mediaDevices ||
        typeof navigator.mediaDevices.enumerateDevices !== 'function') return;

    var BRIDGE = [
        "(function () {",
        "  if (typeof navigator === 'undefined') return;",
        "  if (navigator.mediaDevices && typeof navigator.mediaDevices.enumerateDevices === 'function' &&",
        "      typeof navigator.mediaDevices.getUserMedia === 'function') return;",
        "  var seq = 0, pending = {};",
        "  function call(method, args) {",
        "    return new Promise(function (resolve, reject) {",
        "      var id = ++seq;",
        "      pending[id] = { resolve: resolve, reject: reject };",
        "      self.postMessage({ __spiderMediaRPC: true, id: id, method: method, args: args });",
        "    });",
        "  }",
        "  self.addEventListener('message', function (e) {",
        "    var d = e && e.data;",
        "    if (!d || d.__spiderMediaRPCResult !== true) return;",
        "    var p = pending[d.id];",
        "    if (!p) return;",
        "    delete pending[d.id];",
        "    function mkStream(desc) {",
        "      var ts = (desc.tracks || []).map(function (t) {",
        "        return {",
        "          id: t.id, kind: t.kind, label: t.label, readyState: t.readyState,",
        "          muted: t.muted, enabled: true,",
        "          getSettings: function () { return t.settings || {}; },",
        "          getCapabilities: function () { return {}; },",
        "          stop: function () {",
        "            self.postMessage({ __spiderMediaStop: true, trackId: t.id });",
        "          }",
        "        };",
        "      });",
        "      return {",
        "        active: true,",
        "        id: desc.id || 'spider',",
        "        getTracks: function () { return ts.slice(); },",
        "        getVideoTracks: function () { return ts.filter(function (t) { return t.kind === 'video'; }); },",
        "        getAudioTracks: function () { return ts.filter(function (t) { return t.kind === 'audio'; }); },",
        "        getTrackById: function (i) { return ts.filter(function (t) { return t.id === i; })[0] || null; },",
        "        addEventListener: function () {}, removeEventListener: function () {},",
        "        dispatchEvent: function () { return false; }",
        "      };",
        "    }",
        "    if (d.error) { var err = new Error(d.error.message || ''); err.name = d.error.name || 'Error'; p.reject(err); }",
        "    else if (d.result && d.result.__spiderMediaStream) p.resolve(mkStream(d.result));",
        "    else p.resolve(d.result);",
        "  });",
        "  Object.defineProperty(self.navigator, 'mediaDevices', { value: {",
        "    enumerateDevices: function () { return call('enumerateDevices', []); },",
        "    getUserMedia: function (c) { return call('getUserMedia', [c]); },",
        "    getSupportedConstraints: function () { return {}; },",
        "    ondevicechange: null",
        "  }, configurable: true });",
        "})();"
    ].join('\n');

    var streamRefs = {};
    function cloneForWorker(res) {
        try {
            if (typeof MediaStream !== 'undefined' && res instanceof MediaStream) {
                var id = 's' + Math.random().toString(36).slice(2);
                streamRefs[id] = res;
                return { __spiderMediaStream: true, id: id,
                    tracks: res.getTracks().map(function (t) {
                        return { id: t.id, kind: t.kind, label: t.label,
                            readyState: t.readyState, muted: t.muted,
                            settings: (t.getSettings && t.getSettings()) || {} };
                    }) };
            }
            if (Array.isArray(res)) return res.map(function (d) {
                return { deviceId: d.deviceId, groupId: d.groupId,
                    kind: d.kind, label: d.label };
            });
            return res;
        } catch (e) { return null; }
    }

    function attachRPC(worker) {
        try {
            worker.addEventListener('message', function (ev) {
                var d = ev && ev.data;
                if (d && d.__spiderMediaStop === true) {
                    Object.keys(streamRefs).forEach(function (k) {
                        streamRefs[k].getTracks().forEach(function (t) {
                            if (t.id === d.trackId) { t.stop(); delete streamRefs[k]; }
                        });
                    });
                    return;
                }
                if (!d || d.__spiderMediaRPC !== true) return;
                var fn = navigator.mediaDevices[d.method];
                if (typeof fn !== 'function') {
                    worker.postMessage({ __spiderMediaRPCResult: true, id: d.id,
                        error: { name: 'NotSupportedError', message: String(d.method) + ' unavailable' } });
                    return;
                }
                fn.apply(navigator.mediaDevices, d.args || []).then(function (res) {
                    worker.postMessage({ __spiderMediaRPCResult: true, id: d.id, result: cloneForWorker(res) });
                }).catch(function (err) {
                    worker.postMessage({ __spiderMediaRPCResult: true, id: d.id,
                        error: { name: err && err.name || 'Error', message: err && err.message || '' } });
                });
            });
        } catch (e) {}
        return worker;
    }

    // 1) Prefix JavaScript blobs (covers blob workers, classic and module)
    if (typeof Blob === 'function') {
        var OrigBlob = Blob;
        var SpiderBlob = function (chunks, opts) {
            var t = (opts && opts.type) || '';
            var isArrayLike = chunks && typeof chunks.length === 'number' &&
                !(chunks instanceof ArrayBuffer) &&
                !(typeof ArrayBuffer !== 'undefined' && ArrayBuffer.isView && ArrayBuffer.isView(chunks));
            if (/javascript/i.test(t) && isArrayLike) {
                var parts = [BRIDGE];
                for (var i = 0; i < chunks.length; i++) parts.push(chunks[i]);
                return new OrigBlob(parts, opts);
            }
            return new OrigBlob(chunks, opts);
        };
        SpiderBlob.prototype = OrigBlob.prototype;
        try { window.Blob = SpiderBlob; } catch (e) {}
    }

    // 2) Wrap Worker so RPC replies reach each worker instance. URL workers
    //    are passed through untouched (no re-hosting: it breaks relative
    //    imports); blob workers were already prefixed above.
    var OrigWorker = Worker;
    var SpiderWorker = function (url, options) {
        return attachRPC(new OrigWorker(url, options));
    };
    SpiderWorker.prototype = OrigWorker.prototype;
    try { window.Worker = SpiderWorker; } catch (e) {}

    // Make the wrappers indistinguishable from natives for code that
    // inspects fn.toString()/fn.name before booting.
    function disguise(fn, name) {
        try {
            Object.defineProperty(fn, 'name', { value: name });
            var src = 'function ' + name + '() { [native code] }';
            Object.defineProperty(fn, 'toString', { value: function () { return src; } });
        } catch (e) {}
    }
    try { disguise(SpiderBlob, 'Blob'); } catch (e) {}
    try { disguise(SpiderWorker, 'Worker'); } catch (e) {}
})();
"#;
            // Escape hatch: SPIDER_NO_WORKER_SHIM=1 disables the worker
            // mediaDevices bridge for debugging.
            let ua_ch_shim = if std::env::var_os("SPIDER_NO_WORKER_SHIM").is_none() {
                format!("{}\n{}", ua_ch_shim, worker_shim)
            } else {
                ua_ch_shim
            };
            let shim_script = webkit::UserScript::new(
                &ua_ch_shim,
                webkit::UserContentInjectedFrames::AllFrames,
                webkit::UserScriptInjectionTime::Start,
                &[],
                &[],
            );
            content_manager.add_script(&shim_script);

            // Build WebContext
            let web_context = WebContext::new();
            web_context.set_spell_checking_enabled(true);
            web_context.set_spell_checking_languages(&["en_US"]); // TODO: detect system language and put here

            // Build WebView
            let webview = WebView::builder()
                .network_session(&network_session)
                .settings(&settings)
                .user_content_manager(&content_manager)
                .web_context(&web_context)
                .build();

            // Set to true once the start page finished loading; the very
            // first navigation is always allowed even under domain
            // restriction
            let initial_load_done = std::rc::Rc::new(Cell::new(false));

            {
                let initial_load_done = initial_load_done.clone();
                webview.connect_load_changed(move |_, event| {
                    if event == LoadEvent::Finished {
                        initial_load_done.set(true);
                    }
                });
            }


            {
                let allowed_domains = details.allowed_domains.clone();
                let initial_load_done = initial_load_done.clone();
                webview.connect_decide_policy(move |webview, decision, decision_type| -> bool {
                    decide_policy(
                        &allowed_domains,
                        &initial_load_done,
                        webview,
                        decision,
                        decision_type,
                    )
                });
            }

            // Script-initiated pop ups (window.open). Links that trigger a
            // NewWindowAction policy are opened externally in
            // decide_policy instead.
            {
                let allowed_domains = details.allowed_domains.clone();
                let parent = self.obj().downgrade();
                webview.connect_create(move |_, navigation_action| -> Option<gtk::Widget> {
                    create_popup(
                        &allowed_domains,
                        &parent,
                        &network_session,
                        &web_context,
                        &settings,
                        navigation_action,
                    )
                });
            }

            // Website permissions: apply previously saved decisions
            // without prompting, and ask the user otherwise. Decisions are
            // stored per app + origin so they can be revoked later from
            // the app's settings page.
            {
                let id = details.id.clone();
                let parent = self.obj().downgrade();
                let id_state = id.clone();
                webview.connect_query_permission_state(move |webview, query| -> bool {
                    query_permission_state(&id_state, webview, query)
                });
                webview.connect_permission_request(move |webview, request| -> bool {
                    permission_request(&id, &parent, webview, request)
                });
            }

            // Forward page notifications to the notification daemon;
            // activating one presents this window and replays the click
            // into the page
            {
                let id = details.id.clone();
                let parent = self.obj().downgrade();
                webview.connect_show_notification(move |_, notification| {
                    let Some(window) = parent.upgrade() else {
                        return false;
                    };
                    let Some(application) = window.application() else {
                        return false;
                    };

                    let desktop =
                        gio::Notification::new(&notification.title().unwrap_or_default());
                    desktop.set_body(Some(&notification.body().unwrap_or_default()));
                    desktop.set_default_action_and_target_value(
                        "app.open-app",
                        Some(&id.to_variant()),
                    );
                    application.send_notification(None, &desktop);

                    let mut pending = window.imp().pending_notifications.borrow_mut();
                    if pending.len() > 8 {
                        pending.remove(0);
                    }
                    pending.push(notification.clone());

                    true
                });
            }

            webview.connect_estimated_load_progress_notify(clone!(
                #[weak(rename_to=_self)]
                self,
                move |webview: &WebView| {
                    let progress = webview.estimated_load_progress();
                    _self
                        .progress_bar
                        .set_fraction(if progress == 1.0 { 0.0 } else { progress });
                }
            ));

            webview.connect_uri_notify(clone!(
                #[weak(rename_to=_self)]
                self,
                move |webview: &WebView| {
                    _self.update_nav_buttons(webview);
                }
            ));

            webview
        }

        pub fn go_back(&self) {
            let webview = self.webview.borrow();
            webview.go_back();
            self.update_nav_buttons(&webview);
        }
        pub fn go_forward(&self) {
            let webview = self.webview.borrow();
            webview.go_forward();
            self.update_nav_buttons(&webview);
        }

        fn update_nav_buttons(&self, webview: &WebView) {
            // The back_forward_list hasn't updated by this point
            // We correct by comparing the current uri with where it was in the list
            // This is probably rife with edge cases lol
            let list = webview.back_forward_list().unwrap();

            if list.back_item().and_then(|x| x.uri()) == webview.uri() {
                self.back_button.set_sensitive(list.back_list().len() != 1);
                self.forward_button
                    .set_sensitive(list.forward_list().is_empty());
            } else if list.forward_item().and_then(|x| x.uri()) == webview.uri() {
                self.back_button.set_sensitive(list.back_list().is_empty());
                self.forward_button
                    .set_sensitive(list.forward_list().len() != 1);
            } else {
                self.forward_button.set_sensitive(webview.can_go_forward());
                self.back_button.set_sensitive(webview.can_go_back());
            }
        }
    }

    #[gtk::template_callbacks]
    impl AppWindow {
        #[template_callback]
        fn on_back_clicked(&self, _: gtk::Button) {
            self.go_back()
        }
        #[template_callback]
        fn on_forward_clicked(&self, _: gtk::Button) {
            self.go_forward();
        }
    }
}

glib::wrapper! {
    pub struct AppWindow(ObjectSubclass<imp::AppWindow>)
        @extends adw::ApplicationWindow, gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gtk::Accessible, gtk::Actionable, gtk::Buildable, gtk::ConstraintTarget, gio::ActionMap, gio::ActionGroup, gtk::Native, gtk::ShortcutManager, gtk::Root;
}

impl AppWindow {
    pub fn new<P: IsA<gtk::Application>>(application: &P, details: &AppDetails) -> Self {
        let obj: Self = glib::Object::builder()
            .property("application", application)
            .build();
        obj.imp().set_details(details);
        obj
    }

    pub fn id(&self) -> String {
        self.imp().details.borrow().id.clone()
    }

    /// Replays clicks on activated desktop notifications into the page.
    pub fn activate_pending_notifications(&self) {
        for notification in self.imp().pending_notifications.borrow_mut().drain(..) {
            notification.clicked();
        }
    }

    fn setup_gactions(&self) {
        self.add_action_entries([
            gio::ActionEntry::builder("forward")
                .activate(move |win: &Self, _, _| win.imp().go_forward())
                .build(),
            gio::ActionEntry::builder("back")
                .activate(move |win: &Self, _, _| win.imp().go_back())
                .build(),
            gio::ActionEntry::builder("reload")
                .activate(move |win: &Self, _, _| {
                    win.imp().webview.borrow().reload();
                })
                .build(),
            gio::ActionEntry::builder("reload-bypass-cache")
                .activate(move |win: &Self, _, _| {
                    win.imp().webview.borrow().reload_bypass_cache();
                })
                .build(),
            gio::ActionEntry::builder("stop")
                .activate(move |win: &Self, _, _| {
                    win.imp().webview.borrow().stop_loading();
                })
                .build(),
            gio::ActionEntry::builder("go-home")
                .activate(move |win: &Self, _, _| {
                    let imp = win.imp();
                    let url = imp.details.borrow().url.clone();
                    imp.webview.borrow().load_uri(url.as_str());
                })
                .build(),
            gio::ActionEntry::builder("zoom-in")
                .activate(move |win: &Self, _, _| win.zoom(1.2))
                .build(),
            gio::ActionEntry::builder("zoom-out")
                .activate(move |win: &Self, _, _| win.zoom(1.0 / 1.2))
                .build(),
            gio::ActionEntry::builder("zoom-reset")
                .activate(move |win: &Self, _, _| {
                    win.imp().webview.borrow().set_zoom_level(1.0);
                })
                .build(),
        ]);
    }
    fn zoom(&self, factor: f64) {
        let webview = self.imp().webview.borrow();
        // WebKit's zoom level is relative to a default of 1.0
        const MIN_ZOOM: f64 = 0.25;
        const MAX_ZOOM: f64 = 5.0;
        let new_zoom = (webview.zoom_level() * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        webview.set_zoom_level(new_zoom);
    }
    fn setup_gestures(&self) {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(0);

        // Prevents children (the webview) from seeing the Claimed events
        gesture.set_propagation_phase(gtk::PropagationPhase::Capture);

        // Handle back (8) and forward (9) mouse button events
        gesture.connect_pressed(clone!(
            #[weak(rename_to=_self)]
            self,
            move |gesture, _, _, _| {
                if gesture.current_button() == 8 {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    _self.imp().go_back();
                }
                if gesture.current_button() == 9 {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    _self.imp().go_forward();
                }
            }
        ));

        self.add_controller(gesture);
    }
    fn load_window_size(&self) {
        let details = self.imp().details.borrow();
        self.set_default_size(details.window_width, details.window_height);

        if details.window_maximize {
            self.maximize();
        }
    }
}

fn decide_policy(
    allowed_domains: &Option<Vec<String>>,
    initial_load_done: &Rc<Cell<bool>>,
    webview: &WebView,
    decision: &webkit::PolicyDecision,
    decision_type: PolicyDecisionType,
) -> bool {
    match decision_type {
        PolicyDecisionType::NewWindowAction => {
            // Open anything targeting a new window in the system browser
            // instead
            if let Some(uri) = navigation_action_uri(decision) {
                let _ = open::that_detached(uri);
                decision.ignore();
                return false;
            }
        }
        PolicyDecisionType::NavigationAction => {
            // Domain restriction: keep the app inside its own domains,
            // everything else is handed to the system browser. The very
            // first navigation (the app's start page) is always allowed.
            if initial_load_done.get() {
                if let Some(uri) = navigation_action_uri(decision) {
                    if !uri_allowed(allowed_domains, &uri) {
                        let _ = open::that_detached(uri);
                        decision.ignore();
                        return false;
                    }
                }
            }
        }
        PolicyDecisionType::Response => {
            if let Some(headers) = decision
                .clone()
                .downcast::<webkit::ResponsePolicyDecision>()
                .ok()
                .and_then(|x| x.response())
                .and_then(|x| x.http_headers())
            {
                if headers
                    .one("Content-Type")
                    .and_then(|x| {
                        if webview.can_show_mime_type(x.as_str()) {
                            Some(())
                        } else {
                            None
                        }
                    })
                    .is_none()
                {
                    decision.download();
                    return true;
                }
            }
        }
        _ => (),
    }
    true
}

/// The target URI of a navigation policy decision, if any.
fn navigation_action_uri(decision: &webkit::PolicyDecision) -> Option<String> {
    decision
        .clone()
        .downcast::<webkit::NavigationPolicyDecision>()
        .ok()
        .and_then(|x| x.navigation_action())
        .and_then(|x| x.request())
        .and_then(|x| x.uri())
        .map(|x| x.to_string())
}

#[allow(clippy::too_many_arguments)]
fn create_popup(
    allowed_domains: &Option<Vec<String>>,
    parent: &glib::WeakRef<AppWindow>,
    network_session: &webkit::NetworkSession,
    web_context: &WebContext,
    settings: &webkit::Settings,
    navigation_action: &webkit::NavigationAction,
) -> Option<gtk::Widget> {
    let uri = navigation_action.request().and_then(|r| r.uri())?;
    if !uri_allowed(allowed_domains, &uri) {
        // Restricted app trying to pop up an outside page: hand it to the
        // system browser and suppress the pop up here
        let _ = open::that_detached(uri);
        return None;
    }

    let parent = parent.upgrade()?;

    // A fresh content manager per pop up; the shared context, session and
    // settings keep logins etc. working inside it
    let popup_webview = WebView::builder()
        .network_session(network_session)
        .settings(settings)
        .web_context(web_context)
        .user_content_manager(&webkit::UserContentManager::new())
        .build();

    let application = parent.application()?;
    let window = adw::ApplicationWindow::new(&application);
    window.set_default_size(640, 480);
    window.set_title(Some("Pop Up"));
    window.set_transient_for(Some(&parent));

    let headerbar = adw::HeaderBar::new();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&headerbar);
    toolbar.set_content(Some(&popup_webview));
    window.set_content(Some(&toolbar));

    // Follow the pop up's document title
    popup_webview.connect_title_notify(clone!(
        #[weak]
        window,
        move |webview| {
            let title = webview.title().unwrap_or_else(|| "Pop Up".into());
            window.set_title(Some(title.as_str()));
        }
    ));

    window.present();

    // WebKit loads the requested URI into the returned view itself
    Some(popup_webview.upcast())
}

/// Applies a saved permission decision without prompting.
fn query_permission_state(
    id: &str,
    webview: &WebView,
    query: &webkit::PermissionStateQuery,
) -> bool {
    let Some(kind) = normalize_permission_name(query.name().as_deref()) else {
        return false;
    };
    let Some(origin) = page_origin(webview.uri().as_deref()) else {
        return false;
    };
    match get_app_permission(id, &origin, kind) {
        Some(true) => query.finish(webkit::PermissionState::Granted),
        Some(false) => query.finish(webkit::PermissionState::Denied),
        None => return false,
    }
    true
}

/// Prompts for a website permission request and remembers the answer so it
/// can later be revoked from the app's settings page.
fn permission_request(
    id: &str,
    parent: &glib::WeakRef<AppWindow>,
    webview: &WebView,
    request: &webkit::PermissionRequest,
) -> bool {
    let kinds = permission_kinds(request);
    if kinds.is_empty() {
        // Unknown permission type; fall back to WebKit's default behavior
        return false;
    }
    let Some(origin) = page_origin(webview.uri().as_deref()) else {
        request.deny();
        return true;
    };

    // Already decided?
    let decisions: Vec<Option<bool>> = kinds
        .iter()
        .map(|kind| get_app_permission(id, &origin, kind))
        .collect();
    if decisions.iter().all(|d| d.is_some()) {
        let allow = decisions.into_iter().flatten().all(|v| v);
        if allow {
            request.allow();
        } else {
            request.deny();
        }
        return true;
    }

    let title = get_app_details(id).map(|d| d.title).unwrap_or_default();
    let kind_label = kinds
        .iter()
        .map(|kind| permission_label(kind))
        .collect::<Vec<_>>()
        .join(", ");
    let dialog = adw::AlertDialog::new(
        Some(format!("Allow {title} to use {kind_label}?").as_str()),
        Some(origin.as_str()),
    );
    dialog.add_responses(&[("deny", "Deny"), ("allow", "Allow")]);
    dialog.set_response_appearance("deny", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("allow"));
    dialog.present(parent.upgrade().as_ref());

    let request = request.clone();
    let id = id.to_string();
    dialog.connect_response(None, move |_, response| {
        let allow = response == "allow";
        for kind in &kinds {
            let _ = set_app_permission(&id, &origin, kind, allow);
        }
        if allow {
            request.allow();
        } else {
            request.deny();
        }
    });
    true
}
