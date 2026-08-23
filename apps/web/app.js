(() => {
  const nav = document.querySelector("[data-nav]");
  const onScroll = () => {
    if (!nav) return;
    nav.classList.toggle("is-stuck", window.scrollY > 12);
  };
  onScroll();
  window.addEventListener("scroll", onScroll, { passive: true });

  startLife(document.getElementById("life"));
  wireWaitlist(document.getElementById("waitlist-form"));
})();

function startLife(canvas) {
  if (!canvas) return;
  const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const ctx = canvas.getContext("2d", { alpha: true });
  const cell = 14;
  let cols = 0;
  let rows = 0;
  let grid = [];
  let raf = 0;
  let last = 0;
  const interval = 220;

  const empty = () => Array.from({ length: rows }, () => Array(cols).fill(0));

  const stamp = (pattern, x, y) => {
    for (const [dx, dy] of pattern) {
      const cx = (x + dx + cols) % cols;
      const cy = (y + dy + rows) % rows;
      if (grid[cy]) grid[cy][cx] = 1;
    }
  };

  // Conway glider, as drawn in the mark.
  const glider = [
    [1, 0],
    [2, 1],
    [0, 2],
    [1, 2],
    [2, 2],
  ];
  const lwss = [
    [1, 0],
    [4, 0],
    [0, 1],
    [0, 2],
    [4, 2],
    [0, 3],
    [1, 3],
    [2, 3],
    [3, 3],
  ];

  const seed = () => {
    grid = empty();
    const n = Math.max(4, Math.floor((cols * rows) / 900));
    for (let i = 0; i < n; i++) {
      stamp(glider, Math.floor(Math.random() * cols), Math.floor(Math.random() * rows));
    }
    stamp(lwss, Math.floor(cols * 0.15), Math.floor(rows * 0.35));
    stamp(lwss, Math.floor(cols * 0.62), Math.floor(rows * 0.12));
  };

  const resize = () => {
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    const w = canvas.clientWidth;
    const h = canvas.clientHeight;
    canvas.width = Math.floor(w * dpr);
    canvas.height = Math.floor(h * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    cols = Math.ceil(w / cell);
    rows = Math.ceil(h / cell);
    seed();
    draw();
  };

  const step = () => {
    const next = empty();
    for (let y = 0; y < rows; y++) {
      for (let x = 0; x < cols; x++) {
        let n = 0;
        for (let dy = -1; dy <= 1; dy++) {
          for (let dx = -1; dx <= 1; dx++) {
            if (dx === 0 && dy === 0) continue;
            const nx = (x + dx + cols) % cols;
            const ny = (y + dy + rows) % rows;
            n += grid[ny][nx];
          }
        }
        next[y][x] = n === 3 || (grid[y][x] && n === 2) ? 1 : 0;
      }
    }
    grid = next;
  };

  const draw = () => {
    const w = canvas.clientWidth;
    const h = canvas.clientHeight;
    ctx.clearRect(0, 0, w, h);
    ctx.fillStyle = "rgba(45, 212, 167, 0.55)";
    for (let y = 0; y < rows; y++) {
      for (let x = 0; x < cols; x++) {
        if (!grid[y][x]) continue;
        ctx.beginPath();
        ctx.roundRect(x * cell + 2, y * cell + 2, cell - 4, cell - 4, 2);
        ctx.fill();
      }
    }
  };

  const tick = (t) => {
    if (t - last > interval) {
      step();
      draw();
      last = t;
    }
    raf = requestAnimationFrame(tick);
  };

  resize();
  window.addEventListener("resize", resize);
  if (!reduce) raf = requestAnimationFrame(tick);
}

function wireWaitlist(form) {
  if (!form) return;
  const status = form.querySelector(".form-status");
  const done = document.querySelector(".form-done");
  const email = form.querySelector('input[name="email"]');

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    status.textContent = "";

    const address = (email.value || "").trim();
    if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(address)) {
      status.textContent = "That does not look like an email address.";
      email.focus();
      return;
    }

    const role = (form.querySelector('input[name="role"]:checked') || {}).value || "use";
    const note = (form.querySelector('textarea[name="note"]').value || "").trim();
    const payload = {
      email: address,
      role,
      note,
      at: new Date().toISOString(),
      href: location.href,
    };

    const button = form.querySelector('button[type="submit"]');
    button.disabled = true;
    button.textContent = "Sending…";

    try {
      const res = await fetch("/api/waitlist", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(payload),
      });
      if (!res.ok) throw new Error("not-ok");
      reveal();
    } catch {
      // Preview without the local server, or a static host with no function:
      // keep the signup locally so a later export is still possible.
      try {
        const key = "rootmode.waitlist";
        const prev = JSON.parse(localStorage.getItem(key) || "[]");
        prev.push(payload);
        localStorage.setItem(key, JSON.stringify(prev));
      } catch {
        /* private mode — still show success; they asked to be on a list */
      }
      reveal();
    }

    function reveal() {
      form.hidden = true;
      if (done) done.hidden = false;
    }
  });
}
