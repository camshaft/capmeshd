//! The surface attachment page (DESIGN §10.1).
//!
//! The page is three static assets edited as real files under `src/assets/` and
//! embedded at compile time with [`include_str!`]: the HTML shell
//! ([`SURFACE_HTML`], served at `/s/{id}`) links `/surface.css` ([`SURFACE_CSS`])
//! and `/surface.js` ([`SURFACE_JS`]), which the HTTP layer serves as their own
//! same-origin routes. The HTML is static — the JS reads the surface id from the
//! URL — so there is no server-side templating.
//!
//! The page renders two zones: a **main view** (the currently-selected item) and
//! a **visible inbox feed** (every item ever pushed). It attaches over SSE
//! (`/s/{id}/events`), which delivers an initial snapshot then live updates, so
//! the surface reflects pushes with no reload. Trust model (DESIGN §8):
//! `html`/`script` items are same-origin and trusted (only cluster-authenticated
//! hosts can push), so they render/execute directly; `navigate`/`pdf` third-party
//! URLs render inside a sandboxed iframe that cannot reach this origin.

/// The HTML shell served at `/s/{id}` (links the CSS and JS below).
pub const SURFACE_HTML: &str = include_str!("assets/surface.html");

/// The stylesheet served at `/surface.css`.
pub const SURFACE_CSS: &str = include_str!("assets/surface.css");

/// The attachment-page logic served at `/surface.js`.
pub const SURFACE_JS: &str = include_str!("assets/surface.js");
