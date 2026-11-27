// SPDX-License-Identifier: LGPL-3.0-or-later
//
// Full-viewport Terminal-Art Moon, Lapis Night Sky & Thousand Waves canvas.
// Inspired by megumin.tech's spherical ASCII moon + scroll-reactive dawn sky.
// Zero external dependencies; pauses automatically when the tab is hidden
// and honors prefers-reduced-motion.
(function () {
  "use strict";

  var canvas = document.getElementById("sky-canvas");
  if (!canvas || !canvas.getContext) return;
  var ctx = canvas.getContext("2d", { alpha: false }) || canvas.getContext("2d");
  if (!ctx) return;

  var reducedMotion =
    window.matchMedia &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  var MONO_FONT =
    'ui-monospace, SFMono-Regular, "Cascadia Mono", Menlo, Monaco, Consolas, monospace';

  var SYNODIC = 29.53058867;
  function getCurrentLunarPhase() {
    var knownNewMoon = Date.UTC(2000, 0, 6, 18, 14, 0);
    var days = (Date.now() - knownNewMoon) / 86400000;
    return (((days % SYNODIC) + SYNODIC) % SYNODIC) / SYNODIC;
  }

  function getPhaseName(frac) {
    var f = ((frac % 1) + 1) % 1;
    if (f < 0.03 || f > 0.97) return "New Moon";
    if (f < 0.22) return "Waxing Crescent";
    if (f < 0.28) return "First Quarter";
    if (f < 0.47) return "Waxing Gibbous";
    if (f < 0.53) return "Full Moon";
    if (f < 0.72) return "Waning Gibbous";
    if (f < 0.78) return "Last Quarter";
    return "Waning Crescent";
  }

  var phaseReadout = document.getElementById("moon-phase-readout");
  var phaseBtns = document.querySelectorAll("[data-moon-preset]");
  var phaseFrac = getCurrentLunarPhase();
  var targetPhase = phaseFrac;
  var lastPhaseLabel = "";

  Array.prototype.forEach.call(phaseBtns, function (btn) {
    btn.addEventListener("click", function () {
      var p = btn.getAttribute("data-moon-preset");
      targetPhase = p === "live" ? getCurrentLunarPhase() : Number(p);
      Array.prototype.forEach.call(phaseBtns, function (b) {
        b.classList.toggle("active", b === btn);
      });
    });
  });

  var RAMP = " .·:-=+*#%@";
  var MARIA_SPOTS = [
    { lo: -0.42, la: 0.36, r: 0.4, d: 0.3 },
    { lo: 0.22, la: 0.3, r: 0.28, d: 0.26 },
    { lo: 0.46, la: 0.06, r: 0.3, d: 0.28 },
    { lo: -0.74, la: 0.02, r: 0.44, d: 0.26 },
    { lo: 0.6, la: -0.22, r: 0.24, d: 0.22 },
    { lo: -0.2, la: -0.26, r: 0.25, d: 0.2 },
  ];

  var W = 0;
  var H = 0;
  var dpr = 1;
  var cellW = 7;
  var cellH = 12;
  var stars = [];
  var meteors = [];
  var ripples = [];
  var nearStarsX = new Float32Array(7);
  var nearStarsY = new Float32Array(7);

  var moonCacheKey = "";
  var moonCacheCols = 0;
  var moonCacheRows = 0;
  var moonCacheNx = null;
  var moonCacheNy = null;
  var moonCacheNz = null;
  var moonCacheAlbedo = null;
  var moonCacheEdgeScale = null;
  var moonCacheEarthshine = null;

  var smoothScroll = window.scrollY || 0;
  var pointerX = -999;
  var pointerY = -999;
  var rafId = 0;
  var t = 0;

  function initStars() {
    var count = Math.floor((W * H) / 7200);
    var n = Math.min(200, Math.max(70, count));
    stars = [];
    for (var i = 0; i < n; i++) {
      stars.push({
        x: Math.random() * W,
        y: Math.random() * H * 0.84,
        depth: 0.3 + Math.random() * 0.7,
        ch: i % 11 === 0 ? "✦" : i % 5 === 0 ? "+" : i % 2 === 0 ? "·" : ".",
        baseAlpha: 0.14 + Math.random() * 0.32,
        speed: 0.35 + Math.random() * 0.85,
        offset: Math.random() * Math.PI * 2,
        tint: i % 10 === 0 ? "#f2b5c4" : i % 6 === 0 ? "#86bcff" : "#dbeafe",
      });
    }
  }

  function resize() {
    dpr = Math.min(window.devicePixelRatio || 1, 2);
    W = window.innerWidth;
    H = window.innerHeight;
    canvas.width = Math.floor(W * dpr);
    canvas.height = Math.floor(H * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    cellW = W < 640 ? 6 : 7;
    cellH = W < 640 ? 10 : 12;
    initStars();
  }
  resize();
  window.addEventListener("resize", resize, { passive: true });

  function spawnMeteor(startX, startY, isAmbient) {
    var scale = isAmbient
      ? 0.65 + Math.random() * 0.35
      : 0.78 + Math.random() * 0.42;
    var speed = (3.0 + Math.random() * 1.8) * scale;
    var angle = Math.PI * (0.77 + Math.random() * 0.1);
    var isWarm = Math.random() < 0.25;
    var x = startX;
    var y = startY;
    if (x === undefined) {
      x =
        isAmbient && W > 900
          ? Math.random() < 0.65
            ? W * (0.64 + Math.random() * 0.32)
            : W * (0.1 + Math.random() * 0.24)
          : W * (0.28 + Math.random() * 0.66);
    }
    if (y === undefined) {
      y = isAmbient
        ? H * (0.04 + Math.random() * 0.16)
        : H * (0.04 + Math.random() * 0.26);
    }
    meteors.push({
      x: x,
      y: y,
      vx: Math.cos(angle) * speed,
      vy: Math.sin(angle) * speed,
      maxLen: (110 + Math.random() * 80) * scale,
      width: (1.0 + Math.random() * 0.5) * scale,
      age: 0,
      maxAge: 60 + Math.random() * 30,
      isAmbient: Boolean(isAmbient),
      tint: isWarm ? [242, 181, 196] : [147, 197, 253],
    });
  }

  function lunarAlbedo(lon, lat) {
    var maria = 0;
    for (var i = 0; i < MARIA_SPOTS.length; i++) {
      var s = MARIA_SPOTS[i];
      var d = Math.hypot(lon - s.lo, lat - s.la);
      if (d < s.r) {
        var k = Math.cos((d / s.r) * (Math.PI / 2));
        maria += k * k * s.d;
      }
    }
    var micro =
      Math.sin(lon * 11 + lat * 7) * Math.cos(lat * 13 - lon * 5) * 0.06;
    var tycho = Math.hypot(lon + 0.14, lat + 0.66) < 0.13 ? 0.16 : 0;
    return Math.max(0.36, Math.min(1.0, 0.88 - maria + micro + tycho));
  }

  function ensureMoonGridCache(
    moonCx,
    moonCy,
    moonR,
    colMin,
    colMax,
    rowMin,
    rowMax
  ) {
    var cols = Math.max(0, Math.floor((colMax - colMin) / cellW) + 1);
    var rows = Math.max(0, Math.floor((rowMax - rowMin) / cellH) + 1);
    var key =
      moonCx.toFixed(1) +
      "|" +
      moonCy.toFixed(1) +
      "|" +
      moonR.toFixed(1) +
      "|" +
      colMin +
      "|" +
      rowMin +
      "|" +
      cols +
      "|" +
      rows +
      "|" +
      cellW +
      "|" +
      cellH;
    if (key === moonCacheKey) return;
    moonCacheKey = key;
    moonCacheCols = cols;
    moonCacheRows = rows;
    var total = cols * rows;
    if (!moonCacheNx || moonCacheNx.length < total) {
      moonCacheNx = new Float32Array(total);
      moonCacheNy = new Float32Array(total);
      moonCacheNz = new Float32Array(total);
      moonCacheAlbedo = new Float32Array(total);
      moonCacheEdgeScale = new Float32Array(total);
      moonCacheEarthshine = new Float32Array(total);
    }
    var k = 0;
    for (var r = 0; r < rows; r++) {
      var py = rowMin + r * cellH;
      var ny = (py - moonCy) / moonR;
      for (var c = 0; c < cols; c++, k++) {
        var px = colMin + c * cellW;
        var nx = (px - moonCx) / moonR;
        var r2 = nx * nx + ny * ny;
        if (r2 > 1.0) {
          moonCacheNz[k] = -1;
          continue;
        }
        var nz = Math.sqrt(1.0 - r2);
        var lon = Math.atan2(nx, nz);
        var lat = Math.asin(Math.max(-1, Math.min(1, ny)));
        var albedo = lunarAlbedo(lon, lat);
        var edgeFade = r2 > 0.88 ? (1.0 - r2) / 0.12 : 1.0;
        moonCacheNx[k] = nx;
        moonCacheNy[k] = ny;
        moonCacheNz[k] = nz;
        moonCacheAlbedo[k] = albedo;
        moonCacheEdgeScale[k] = 0.42 + 0.58 * edgeFade;
        moonCacheEarthshine[k] = 0.08 * Math.pow(nz, 0.55) * albedo;
      }
    }
  }

  window.addEventListener(
    "pointermove",
    function (e) {
      pointerX = e.clientX;
      pointerY = e.clientY;
    },
    { passive: true }
  );
  window.addEventListener("pointerleave", function () {
    pointerX = -999;
    pointerY = -999;
  });
  document.addEventListener("click", function (e) {
    if (e.target !== canvas && e.target !== document.body) return;
    if (reducedMotion) return;
    ripples.push({ x: e.clientX, y: e.clientY, r: 4, alpha: 0.42 });
    spawnMeteor(e.clientX, e.clientY, false);
  });

  function drawSky() {
    t += reducedMotion ? 0 : 0.012;

    var targetScroll = window.scrollY || 0;
    smoothScroll += (targetScroll - smoothScroll) * (reducedMotion ? 1 : 0.09);
    var maxScroll = Math.max(
      1,
      document.documentElement.scrollHeight - window.innerHeight
    );
    var progress = Math.min(1, Math.max(0, smoothScroll / maxScroll));

    var diff = targetPhase - phaseFrac;
    if (diff > 0.5) diff -= 1;
    if (diff < -0.5) diff += 1;
    phaseFrac = (((phaseFrac + diff * 0.14) % 1) + 1) % 1;

    var sunAng = phaseFrac * Math.PI * 2;
    var lx = Math.sin(sunAng);
    var lz = -Math.cos(sunAng);
    var ly = -0.12;
    var illum = Math.round(((1 - Math.cos(sunAng)) / 2) * 100);
    var phaseLabel = getPhaseName(phaseFrac) + " · " + illum + "%";
    if (phaseReadout && phaseLabel !== lastPhaseLabel) {
      lastPhaseLabel = phaseLabel;
      phaseReadout.textContent = phaseLabel;
    }

    var horizonY = Math.floor(H * 0.84);
    var skyGrad = ctx.createLinearGradient(0, 0, 0, horizonY);
    skyGrad.addColorStop(0, "#050914");
    skyGrad.addColorStop(
      0.55,
      "rgb(" +
        Math.round(10 + progress * 10) +
        ", " +
        Math.round(20 + progress * 18) +
        ", " +
        Math.round(44 + progress * 34) +
        ")"
    );
    skyGrad.addColorStop(
      0.86,
      "rgb(" +
        Math.round(17 + progress * 22) +
        ", " +
        Math.round(33 + progress * 30) +
        ", " +
        Math.round(70 + progress * 42) +
        ")"
    );
    skyGrad.addColorStop(
      1,
      "rgb(" +
        Math.round(25 + progress * 38) +
        ", " +
        Math.round(41 + progress * 28) +
        ", " +
        Math.round(84 + progress * 34) +
        ")"
    );
    ctx.fillStyle = skyGrad;
    ctx.fillRect(0, 0, W, horizonY);

    var seaGrad = ctx.createLinearGradient(0, horizonY, 0, H);
    seaGrad.addColorStop(
      0,
      "rgb(" +
        Math.round(12 + progress * 16) +
        ", " +
        Math.round(24 + progress * 20) +
        ", " +
        Math.round(52 + progress * 26) +
        ")"
    );
    seaGrad.addColorStop(1, "#050914");
    ctx.fillStyle = seaGrad;
    ctx.fillRect(0, horizonY, W, H - horizonY);

    var horizonBlend = ctx.createLinearGradient(
      0,
      horizonY - 34,
      0,
      horizonY + 34
    );
    horizonBlend.addColorStop(0, "rgba(20, 36, 72, 0)");
    horizonBlend.addColorStop(
      0.5,
      "rgba(" +
        Math.round(24 + progress * 30) +
        ", " +
        Math.round(38 + progress * 26) +
        ", " +
        Math.round(76 + progress * 30) +
        ", 0.42)"
    );
    horizonBlend.addColorStop(1, "rgba(12, 24, 52, 0)");
    ctx.fillStyle = horizonBlend;
    ctx.fillRect(0, horizonY - 34, W, 68);

    if (progress > 0.01) {
      ctx.save();
      ctx.translate(W * 0.5, horizonY);
      ctx.scale(1, 0.3);
      var glowR = Math.max(W * 0.6, 400);
      var peachGlow = ctx.createRadialGradient(0, 0, 10, 0, 0, glowR);
      peachGlow.addColorStop(0, "rgba(242, 181, 196, " + 0.09 * progress + ")");
      peachGlow.addColorStop(
        0.55,
        "rgba(242, 181, 196, " + 0.03 * progress + ")"
      );
      peachGlow.addColorStop(1, "rgba(242, 181, 196, 0)");
      ctx.fillStyle = peachGlow;
      ctx.fillRect(-glowR, -glowR, glowR * 2, glowR * 2);
      ctx.restore();
    }

    // Scroll-reactive celestial arc: drifts gently upward and slightly inward as you scroll
    var isMobile = W <= 820;
    var baseMoonX = isMobile ? W * 0.78 : Math.min(W * 0.84, W / 2 + 400);
    var baseMoonY = isMobile ? H * 0.2 : H * 0.34;
    var moonCx = baseMoonX - (reducedMotion ? 0 : progress * (isMobile ? 28 : 84));
    var moonCy = baseMoonY - (reducedMotion ? 0 : progress * (isMobile ? 54 : 112));
    var moonR = isMobile
      ? Math.min(W * 0.21, 88)
      : Math.min(W * 0.125, H * 0.22, 146);

    var haloGrad = ctx.createRadialGradient(
      moonCx,
      moonCy,
      moonR * 0.35,
      moonCx,
      moonCy,
      moonR * 2.2
    );
    var haloAlpha = 0.07 + (illum / 100) * 0.11;
    haloGrad.addColorStop(0, "rgba(219, 234, 254, " + haloAlpha + ")");
    haloGrad.addColorStop(0.5, "rgba(134, 188, 255, " + haloAlpha * 0.36 + ")");
    haloGrad.addColorStop(1, "rgba(5, 9, 20, 0)");
    ctx.fillStyle = haloGrad;
    ctx.fillRect(0, 0, W, H);

    // Terminal-art starfield with scroll parallax
    ctx.font = "11px " + MONO_FONT;
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    var nearStarsCount = 0;

    for (var i = 0; i < stars.length; i++) {
      var s = stars[i];
      var py =
        (((s.y - smoothScroll * (0.045 * s.depth)) % horizonY) + horizonY) %
        horizonY;
      if (Math.hypot(s.x - moonCx, py - moonCy) < moonR * 1.04) continue;

      var distPtr = Math.hypot(s.x - pointerX, py - pointerY);
      var boost = distPtr < 120 ? (1 - distPtr / 120) * 0.5 : 0;
      if (distPtr < 108 && nearStarsCount < 7) {
        nearStarsX[nearStarsCount] = s.x;
        nearStarsY[nearStarsCount] = py;
        nearStarsCount++;
      }

      var tw = 0.75 + 0.25 * Math.sin(t * s.speed + s.offset);
      ctx.fillStyle = boost > 0.2 ? "#f0f4fc" : s.tint;
      ctx.globalAlpha = Math.min(
        0.9,
        s.baseAlpha * tw * (1 - progress * 0.22) + boost
      );
      ctx.fillText(s.ch, s.x, py);
    }

    if (nearStarsCount > 1) {
      ctx.strokeStyle = "rgba(147, 197, 253, 0.15)";
      ctx.lineWidth = 0.8;
      ctx.beginPath();
      for (var a = 0; a < nearStarsCount; a++) {
        for (var b = a + 1; b < nearStarsCount; b++) {
          if (
            Math.hypot(
              nearStarsX[a] - nearStarsX[b],
              nearStarsY[a] - nearStarsY[b]
            ) < 102
          ) {
            ctx.moveTo(nearStarsX[a], nearStarsY[a]);
            ctx.lineTo(nearStarsX[b], nearStarsY[b]);
          }
        }
      }
      ctx.stroke();
    }

    // Spherical Terminal-Art Moon (zero-allocation cached normal & albedo grid)
    ctx.font = cellH - 1 + "px " + MONO_FONT;
    var colMin = Math.floor((moonCx - moonR) / cellW) * cellW;
    var colMax = Math.ceil((moonCx + moonR) / cellW) * cellW;
    var rowMin = Math.floor((moonCy - moonR) / cellH) * cellH;
    var rowMax = Math.ceil((moonCy + moonR) / cellH) * cellH;
    ensureMoonGridCache(moonCx, moonCy, moonR, colMin, colMax, rowMin, rowMax);

    var k = 0;
    for (var r = 0; r < moonCacheRows; r++) {
      var my = rowMin + r * cellH;
      if (my >= horizonY) {
        k += moonCacheCols;
        continue;
      }
      for (var c = 0; c < moonCacheCols; c++, k++) {
        var nz = moonCacheNz[k];
        if (nz < 0) continue;
        var mx = colMin + c * cellW;
        var ndotl = moonCacheNx[k] * lx + moonCacheNy[k] * ly + nz * lz;
        var lit =
          ndotl > 0
            ? Math.pow(ndotl, 0.68) * moonCacheAlbedo[k]
            : moonCacheEarthshine[k];
        var val = Math.max(0, Math.min(1, lit * moonCacheEdgeScale[k]));
        var idx = Math.min(
          RAMP.length - 1,
          Math.max(1, Math.round(val * (RAMP.length - 1)))
        );
        ctx.fillStyle = ndotl > 0 ? "#e6f0ff" : "#64748b";
        ctx.globalAlpha = Math.max(0.14, Math.min(0.94, val * 1.04));
        ctx.fillText(RAMP.charAt(idx), mx, my);
      }
    }

    // Quiet layered ASCII sea waves & moonlight reflection path
    var waveStepX = W < 640 ? 11 : 12;
    var waveRows = Math.max(5, Math.floor((H - horizonY) / 14));
    ctx.font = "11px " + MONO_FONT;

    for (var wr = 0; wr < waveRows; wr++) {
      var depth = wr / Math.max(1, waveRows - 1);
      var wy = horizonY + 8 + wr * 14;
      var spread = 1 + depth * 2.2;
      var freq = 0.014 - depth * 0.005;
      var speed = (0.55 + depth * 0.55) * (wr % 2 === 0 ? 1 : -0.7);

      for (var wx = 0; wx < W; wx += waveStepX) {
        var swell =
          Math.sin(wx * freq + t * speed + wr * 1.7) +
          Math.cos(wx * freq * 1.8 - t * speed * 0.6 + wr) * 0.5;
        if (swell < 0.15) continue;

        var distMoonX = Math.abs(wx - moonCx);
        var moonPath =
          Math.exp(
            -(distMoonX * distMoonX) /
              (2 * Math.pow(moonR * 0.65 * spread, 2))
          ) *
          (0.25 + (illum / 100) * 0.75);

        var ch =
          moonPath > 0.45
            ? "="
            : swell > 0.95
              ? "~"
              : swell > 0.55
                ? "-"
                : "·";
        ctx.fillStyle =
          moonPath > 0.32
            ? "#dbeafe"
            : progress > 0.45 && wr < 2
              ? "#f2b5c4"
              : "#60a5fa";
        ctx.globalAlpha = Math.min(
          0.42,
          0.06 + depth * 0.08 + moonPath * 0.28
        );
        ctx.fillText(ch, wx, wy);
      }
    }

    // Ambient shooting stars
    if (!reducedMotion && Math.random() < 0.0012) {
      spawnMeteor(undefined, undefined, true);
    }
    if (meteors.length > 0) {
      ctx.save();
      ctx.globalCompositeOperation = "lighter";
      var mWrite = 0;
      for (var mi = 0; mi < meteors.length; mi++) {
        var m = meteors[mi];
        m.age += 1;
        var mp = m.age / m.maxAge;
        if (mp >= 1) continue;
        var env =
          (mp < 0.22 ? mp / 0.22 : Math.pow(1 - (mp - 0.22) / 0.78, 1.45)) *
          (m.isAmbient ? 0.68 : 0.95);
        m.x += m.vx * (1 - mp * 0.22);
        m.y += m.vy * (1 - mp * 0.22);
        var spd = Math.hypot(m.vx, m.vy) || 1;
        var ux = m.vx / spd;
        var uy = m.vy / spd;
        var curLen = m.maxLen * Math.min(1, mp * 2.5) * (0.55 + 0.45 * env);
        var tailX = m.x - ux * curLen;
        var tailY = m.y - uy * curLen;
        var tr = m.tint[0];
        var tg = m.tint[1];
        var tb = m.tint[2];

        var grad = ctx.createLinearGradient(m.x, m.y, tailX, tailY);
        grad.addColorStop(0, "rgba(255,255,255," + Math.min(1, env) + ")");
        grad.addColorStop(
          0.35,
          "rgba(" + tr + "," + tg + "," + tb + "," + env * 0.55 + ")"
        );
        grad.addColorStop(1, "rgba(" + tr + "," + tg + "," + tb + ",0)");
        ctx.strokeStyle = grad;
        ctx.lineWidth = m.width * 1.4;
        ctx.lineCap = "round";
        ctx.beginPath();
        ctx.moveTo(m.x, m.y);
        ctx.lineTo(tailX, tailY);
        ctx.stroke();

        if (m.x > -240 && m.y < H + 120) {
          meteors[mWrite++] = m;
        }
      }
      meteors.length = mWrite;
      ctx.restore();
    }

    if (ripples.length > 0) {
      var rWrite = 0;
      for (var ri = 0; ri < ripples.length; ri++) {
        var rp = ripples[ri];
        rp.r += 1.8;
        rp.alpha *= 0.94;
        ctx.strokeStyle = "rgba(147, 197, 253, " + rp.alpha + ")";
        ctx.lineWidth = 1;
        ctx.beginPath();
        ctx.arc(rp.x, rp.y, rp.r, 0, Math.PI * 2);
        ctx.stroke();
        if (rp.alpha > 0.02) ripples[rWrite++] = rp;
      }
      ripples.length = rWrite;
    }

    ctx.globalAlpha = 1;
    if (!document.hidden && !reducedMotion) {
      rafId = window.requestAnimationFrame(drawSky);
    } else {
      rafId = 0;
    }
  }

  document.addEventListener("visibilitychange", function () {
    if (!document.hidden && !rafId && !reducedMotion) {
      rafId = window.requestAnimationFrame(drawSky);
    }
  });
  if (reducedMotion) {
    window.addEventListener("scroll", drawSky, { passive: true });
    window.addEventListener("resize", drawSky, { passive: true });
    drawSky();
  } else {
    rafId = window.requestAnimationFrame(drawSky);
  }
})();
