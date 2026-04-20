/**
 * SpoolEase Translation OAuth Proxy + Submission Worker
 *
 * Endpoints:
 *   POST /device/code   → GitHub Device Flow: request device + user code
 *   POST /device/token  → GitHub Device Flow: poll for access token
 *   POST /submit        → Verify submitter identity, then use the bot token
 *                          to commit a translation file and open a PR.
 *   OPTIONS *           → CORS preflight
 *
 * The submitter's token is used ONLY for identity verification (GET /user).
 * All write operations (branch, commit, PR) use env.BOT_TOKEN stored
 * as a Cloudflare Worker secret — submitters need NO write access to the repo.
 *
 * Setup (one-time, by the repo owner):
 *   1. Create a GitHub OAuth App:
 *      https://github.com/settings/developers → "New OAuth App"
 *      - Application name: SpoolEase Translation Uploader
 *      - Homepage URL: https://mybesttools.github.io/SpoolEase
 *      - Authorization callback URL: https://mybesttools.github.io/SpoolEase
 *        (unused for Device Flow but GitHub requires a value)
 *      - ✅ Enable Device Flow
 *      Note the Client ID — paste it into docs/translations-upload.html.
 *
 *   2. Create a fine-grained PAT on your account:
 *      https://github.com/settings/personal-access-tokens/new
 *      - Resource owner: yanshay
 *      - Repository access: Only SpoolEase
 *      - Permissions: Contents = Read & write, Pull requests = Read & write
 *      Save the token.
 *
 *   3. Deploy this worker and add the required secrets:
 *      npm install -g wrangler
 *      wrangler login
 *      cd workers/translation-oauth
 *      wrangler deploy
 *      wrangler secret put BOT_TOKEN   ← paste the PAT from step 2
 *      wrangler secret put OAUTH_CLIENT_SECRET   ← paste GitHub OAuth app client secret
 *
 *   4. Update OAUTH_CLIENT_ID and OAUTH_WORKER_URL in docs/translations-upload.html.
 */

const ALLOWED_ORIGINS = [
  "https://mybesttools.github.io",
  "http://localhost",
  "http://127.0.0.1",
];

const OWNER       = "mybesttools";
const REPO        = "SpoolEase";
const BASE_BRANCH = "main";
const MIN_ACCOUNT_AGE_DAYS = 7;

const GITHUB_DEVICE_CODE_URL = "https://github.com/login/device/code";
const GITHUB_TOKEN_URL       = "https://github.com/login/oauth/access_token";
const GITHUB_API             = "https://api.github.com";

function makeCorsHeaders(requestOrigin) {
  const allowed = ALLOWED_ORIGINS.some(
    (o) => requestOrigin && requestOrigin.startsWith(o)
  );
  return {
    "Access-Control-Allow-Origin": allowed ? requestOrigin : "null",
    "Access-Control-Allow-Methods": "POST, OPTIONS",
    "Access-Control-Allow-Headers": "Content-Type, Accept",
    "Access-Control-Max-Age": "86400",
  };
}

function jsonResponse(body, status, cors) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json", ...cors },
  });
}

function daysSince(isoDate) {
  return Math.floor((Date.now() - new Date(isoDate).getTime()) / 86400000);
}

async function ghApi(path, botToken, options) {
  const res = await fetch(GITHUB_API + path, {
    method: (options && options.method) || "GET",
    headers: {
      Accept: "application/vnd.github+json",
      Authorization: "Bearer " + botToken,
      "User-Agent": "SpoolEase-Translation-Worker/1.0",
      "X-GitHub-Api-Version": "2022-11-28",
      "Content-Type": "application/json",
    },
    body: options && options.body ? JSON.stringify(options.body) : undefined,
  });
  const text = await res.text();
  let data = {};
  try { data = text ? JSON.parse(text) : {}; } catch (_) { data = { message: text }; }
  if (!res.ok) throw new Error(data.message || "GitHub API error " + res.status);
  return data;
}

async function handleSubmit(request, env, cors) {
  let payload;
  try {
    payload = await request.json();
  } catch (_) {
    return jsonResponse({ error: "invalid_json" }, 400, cors);
  }

  const { user_token, lang_code, file_content, pr_note } = payload;

  if (!user_token || !lang_code || !file_content) {
    return jsonResponse({ error: "missing_fields" }, 400, cors);
  }

  // Validate lang_code to prevent path traversal
  if (!/^[a-z]{2,5}$/.test(lang_code)) {
    return jsonResponse({ error: "invalid_lang_code" }, 400, cors);
  }

  if (!env.BOT_TOKEN) {
    return jsonResponse({ error: "worker_not_configured", message: "BOT_TOKEN secret is not set." }, 503, cors);
  }

  // ── 1. Verify submitter identity (read-only, uses their token) ──────
  let userLogin, userAge;
  try {
    const userRes = await fetch(GITHUB_API + "/user", {
      headers: {
        Accept: "application/vnd.github+json",
        Authorization: "Bearer " + user_token,
        "User-Agent": "SpoolEase-Translation-Worker/1.0",
        "X-GitHub-Api-Version": "2022-11-28",
      },
    });
    if (!userRes.ok) {
      let ghMsg = "HTTP " + userRes.status;
      try { const body = await userRes.json(); ghMsg = body.message || ghMsg; } catch (_) {}
      return jsonResponse({ error: "auth_failed", message: "GitHub identity check failed: " + ghMsg }, 401, cors);
    }
    const user = await userRes.json();
    userLogin = user.login;
    userAge   = daysSince(user.created_at);
  } catch (err) {
    return jsonResponse({ error: "auth_error", message: String(err) }, 502, cors);
  }

  if (userAge < MIN_ACCOUNT_AGE_DAYS) {
    return jsonResponse({
      error: "account_too_new",
      message: `Account is ${userAge} day(s) old. Minimum is ${MIN_ACCOUNT_AGE_DAYS} days.`,
    }, 403, cors);
  }

  // ── 2. Create branch in main repo using bot token ───────────────────
  let baseSha;
  try {
    const ref = await ghApi(`/repos/${OWNER}/${REPO}/git/ref/heads/${BASE_BRANCH}`, env.BOT_TOKEN);
    baseSha = ref.object.sha;
  } catch (err) {
    return jsonResponse({ error: "github_error", message: String(err) }, 502, cors);
  }

  const stamp      = new Date().toISOString().replace(/[-:TZ.]/g, "").slice(0, 14);
  const branchName = `translation-${lang_code}-${stamp}`;

  try {
    await ghApi(`/repos/${OWNER}/${REPO}/git/refs`, env.BOT_TOKEN, {
      method: "POST",
      body: { ref: `refs/heads/${branchName}`, sha: baseSha },
    });
  } catch (err) {
    return jsonResponse({ error: "branch_error", message: String(err) }, 502, cors);
  }

  // ── 3. Commit the translation file ──────────────────────────────────
  const filePath = `core/translations/${lang_code}.json`;

  // Check if file already exists (to get its SHA for update)
  let existingSha;
  try {
    const existing = await ghApi(
      `/repos/${OWNER}/${REPO}/contents/${filePath}?ref=${encodeURIComponent(branchName)}`,
      env.BOT_TOKEN
    );
    existingSha = existing && existing.sha;
  } catch (_) { /* new file */ }

  const commitBody = {
    message: (existingSha ? "Update" : "Add") + ` ${lang_code} translation (submitted by @${userLogin})`,
    content: btoa(unescape(encodeURIComponent(file_content))),
    branch: branchName,
  };
  if (existingSha) { commitBody.sha = existingSha; }

  try {
    await ghApi(`/repos/${OWNER}/${REPO}/contents/${filePath}`, env.BOT_TOKEN, {
      method: "PUT",
      body: commitBody,
    });
  } catch (err) {
    return jsonResponse({ error: "commit_error", message: String(err) }, 502, cors);
  }

  // ── 4. Open the Pull Request ─────────────────────────────────────────
  const prBodyLines = [
    "Submitted via the SpoolEase translation uploader.",
    "",
    `- Language code: \`${lang_code}\``,
    `- Submitted by: @${userLogin}`,
    "- Source: GitHub Pages upload form",
  ];
  if (pr_note && pr_note.trim()) {
    prBodyLines.push("", "Contributor note:", pr_note.trim());
  }

  let pr;
  try {
    pr = await ghApi(`/repos/${OWNER}/${REPO}/pulls`, env.BOT_TOKEN, {
      method: "POST",
      body: {
        title: `Add translation: ${lang_code}`,
        head: branchName,
        base: BASE_BRANCH,
        draft: true,
        body: prBodyLines.join("\n"),
      },
    });
  } catch (err) {
    const msg = String(err.message || "");
    if (msg.toLowerCase().includes("a pull request already exists")) {
      return jsonResponse({ error: "pr_exists" }, 409, cors);
    }
    return jsonResponse({ error: "pr_error", message: msg }, 502, cors);
  }

  return jsonResponse({ pr_url: pr.html_url, pr_number: pr.number }, 200, cors);
}

export default {
  async fetch(request, env) {
    const origin = request.headers.get("Origin") || "";
    const cors = makeCorsHeaders(origin);

    if (request.method === "OPTIONS") {
      return new Response(null, { status: 204, headers: cors });
    }

    if (request.method !== "POST") {
      return new Response("Method not allowed", { status: 405, headers: cors });
    }

    const { pathname } = new URL(request.url);

    if (pathname === "/submit") {
      return handleSubmit(request, env, cors);
    }

    // Device Flow proxy endpoints.
    // For /device/token, we optionally append OAUTH_CLIENT_SECRET server-side.
    let target;
    let body = await request.text();
    if (pathname === "/device/code") {
      target = GITHUB_DEVICE_CODE_URL;
    } else if (pathname === "/device/token") {
      target = GITHUB_TOKEN_URL;
      if (env.OAUTH_CLIENT_SECRET) {
        const params = new URLSearchParams(body);
        if (!params.get("client_secret")) {
          params.set("client_secret", env.OAUTH_CLIENT_SECRET);
          body = params.toString();
        }
      }
    } else {
      return new Response("Not found", { status: 404, headers: cors });
    }

    let upstreamResponse;
    try {
      upstreamResponse = await fetch(target, {
        method: "POST",
        headers: { "Content-Type": "application/x-www-form-urlencoded", Accept: "application/json" },
        body,
      });
    } catch (err) {
      return new Response(
        JSON.stringify({ error: "upstream_error", error_description: String(err) }),
        { status: 502, headers: { "Content-Type": "application/json", ...cors } }
      );
    }

    const text = await upstreamResponse.text();
    return new Response(text, {
      status: upstreamResponse.status,
      headers: { "Content-Type": "application/json", ...cors },
    });
  },
};
