// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//
// Single-source brand configuration for the mc-spine landing page.
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
    projectName: "mc-spine",

    // Prefix every CLI binary shares: `<cliPrefix>-init`,
    // `<cliPrefix>-mod-check`, `<cliPrefix>-world-migrate`,
    // `<cliPrefix>-client`, and the bare `<cliPrefix>` server binary.
    cliPrefix: "mc-spine",

    // Short line under the project name in the nav/hero.
    tagline: "A Rust-native Minecraft server & client runtime",

    // Badge shown next to the project name.
    statusBadge: "Experimental Alpha",

    // One-sentence value proposition in the hero.
    valueProp:
      "A Rust-native Minecraft server & client runtime with tiered " +
      "NeoForge/Fabric mod compatibility and Velocity forwarding support.",

    // Minecraft versions this runtime targets. The first entry is the LTS
    // target and the pre-flight checker's default; the last is labelled
    // "Latest". Keep in sync with McVersion::V1_21_1 / McVersion::V26_3.
    mcTargets: ["1.21.1", "26.3"],

    // Canonical repository and derived links.
    githubUrl: "https://github.com/nekomario28/mc-spine",
    releasesUrl: "https://github.com/nekomario28/mc-spine/releases",
    docsBaseUrl: "https://github.com/nekomario28/mc-spine/tree/main/docs",

    // Optional custom domain once the brand name is final, e.g.
    // "megu.megumin.tech". Leave "" while undecided: the footer deploy note
    // stays hidden until a domain is set.
    customDomain: "",

    // Default classification sets for the browser pre-flight checker,
    // mirroring the `mc-spine-mod-check` CLI defaults
    // (--flagship-mods create, no --profiles-dir).
    flagshipMods: ["create"],
    profileMods: [],
  };

  root.SITE_CONFIG = SITE_CONFIG;
  if (typeof module !== "undefined" && module.exports) {
    module.exports = SITE_CONFIG;
  }
})(typeof window !== "undefined" ? window : globalThis);
