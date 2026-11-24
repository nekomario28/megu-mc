// SPDX-License-Identifier: LGPL-3.0-or-later
//
// Single-source brand configuration for the megu landing page.
//
// Everything brandable on index.html binds from this ONE object: rename the
// project, swap the tagline, point at a different repo, or set a custom
// domain (<name>.megumin.tech) and every mention, CLI example, link, and
// pill on the page follows — no other file needs an edit.
//
// index.html ships sensible static fallback text, so the page stays fully
// readable even when this file (or JavaScript) is unavailable.
(function (root) {
  "use strict";

  var SITE_CONFIG = {
    // Display name of the project (hero, nav, footer, <title>).
    projectName: "Megu",

    // Prefix every CLI binary shares: `<cliPrefix>-init`,
    // `<cliPrefix>-mod-check`, `<cliPrefix>-world-migrate`,
    // `<cliPrefix>-client`, and the bare `<cliPrefix>` server binary.
    // Kept at "megu" while binaries/packages remain on the development
    // codename (provisional brand preview only).
    cliPrefix: "megu",

    // Short line under the project name in the nav/hero.
    tagline: "A Minecraft-compatible engine written in Rust (codename: megu)",

    // Badge shown next to the project name.
    statusBadge: "Experimental Alpha",

    // One-sentence value proposition in the hero.
    valueProp:
      "A Rust-native Minecraft server & client runtime with tiered " +
      "NeoForge/Fabric mod compatibility and Velocity forwarding support.",

    // Minecraft versions this runtime targets. Currently focused on 1.21.1.
    mcTargets: ["1.21.1"],

    // Canonical repository and derived links.
    githubUrl: "https://github.com/nekomario28/megu-mc",
    releasesUrl: "https://github.com/nekomario28/megu-mc/releases",

    // Pinned release tag behind every static download link in index.html.
    // GitHub's /releases/latest redirect only resolves to a *stable* release,
    // so while the project ships prereleases (alpha/beta) it returns 404 and
    // every "latest/download" href would be dead without JavaScript. The
    // static hrefs therefore point at this exact published tag, and app.js
    // upgrades them to /api/latest when that endpoint is reachable. Bump this
    // and the matching index.html hrefs together; website/check_site.py fails
    // closed when they disagree.
    releaseTag: "v0.1.0-alpha.3",
    docsBaseUrl: "https://github.com/nekomario28/megu-mc/tree/main/docs",

    // Optional custom domain once the brand name is final, e.g.
    // "megu.megumin.tech". Leave "" while undecided: the footer deploy note
    // stays hidden until a domain is set.
    customDomain: "megu.megumin.tech",

    // Default classification sets for the browser pre-flight checker,
    // mirroring the `megu-mod-check` CLI defaults
    // (--flagship-mods create, no --profiles-dir).
    flagshipMods: ["create"],
    profileMods: [],

    // Report endpoint and optional community/support/partner links.
    // Every support/partner link is single-source: change a URL here later
    // (e.g. Ko-fi -> GitHub Sponsors -> Certified Host) with zero HTML edits
    // and without re-shipping the Megu client binary (client points to
    // https://megu.megumin.tech/#support).
    //
    // reportApiUrl       — Cloudflare Pages Functions endpoint
    // turnstileSiteKey   — public Cloudflare Turnstile widget key (never a secret)
    // discussionsUrl     — GitHub Discussions (or forum) for ideas & support
    // discordUrl         — Discord invite, when the community server exists
    // sponsorUrl         — primary support link in the footer
    // githubSponsorsUrl  — GitHub Sponsors page (0% fee, Stripe JP bank payout)
    // kofiUrl            — Ko-fi tip/membership page (0% fee, global MC users)
    // ofuseUrl           — OFUSE / FANBOX page (JP domestic support)
    // partnerEmail       — Cloudflare Email Routing alias (e.g. partners@megumin.tech)
    // certifiedHostName  — optional recommended Megu Certified Host name
    // certifiedHostUrl   — optional partner/affiliate link for one-click hosting
    reportApiUrl: "/api/report",
    latestApiUrl: "/api/latest",
    turnstileSiteKey: "0x4AAAAAAGNFPiu-LWbjNy5A",
    discussionsUrl: "https://github.com/nekomario28/megu-mc/discussions",
    discordUrl: "",
    sponsorUrl: "https://github.com/sponsors/nekomario28",
    githubSponsorsUrl: "https://github.com/sponsors/nekomario28",
    kofiUrl: "",
    ofuseUrl: "",
    partnerEmail: "partners@megumin.tech",
    certifiedHostName: "",
    certifiedHostUrl: "",
  };

  root.SITE_CONFIG = SITE_CONFIG;
  if (typeof module !== "undefined" && module.exports) {
    module.exports = SITE_CONFIG;
  }
})(typeof window !== "undefined" ? window : globalThis);
