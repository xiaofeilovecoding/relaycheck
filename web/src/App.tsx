import { useCallback, useEffect, useRef, useState } from "react";
import { api, ApiError, setCsrf } from "./api";
import { detectLang, dict, Lang, storeLang } from "./i18n";
import { TaskDetail } from "./TaskDetail";
import { BuckleMark } from "./components";

export type Theme = "system" | "light" | "dark";

function applyTheme(theme: Theme) {
  let dark: boolean;
  if (theme === "system") {
    dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
  } else {
    dark = theme === "dark";
  }
  document.documentElement.dataset.theme = dark ? "dark" : "light";
}

function loadTheme(): Theme {
  try {
    const t = localStorage.getItem("rc.theme");
    if (t === "light" || t === "dark" || t === "system") return t;
  } catch {
    /* ignore */
  }
  return "system";
}

export function App() {
  const [lang, setLangState] = useState<Lang>(detectLang);
  const [theme, setThemeState] = useState<Theme>(loadTheme);
  const [session, setSession] = useState<"pending" | "ok" | "missing">("pending");
  const [state, setState] = useState<any>(null);
  const [route, setRoute] = useState<{ view: "handoffs" | "tasks" | "settings"; taskId: string | null }>({
    view: "handoffs",
    taskId: null,
  });
  const eventsHead = useRef(0);
  const [detailTick, setDetailTick] = useState(0);
  const t = dict[lang];

  const setLang = (l: Lang) => {
    setLangState(l);
    storeLang(l);
    document.documentElement.lang = l === "zh" ? "zh-CN" : "en";
  };
  const setTheme = (th: Theme) => {
    setThemeState(th);
    try {
      localStorage.setItem("rc.theme", th);
    } catch {
      /* ignore */
    }
    applyTheme(th);
  };

  useEffect(() => {
    applyTheme(theme);
    document.documentElement.lang = lang === "zh" ? "zh-CN" : "en";
  }, [theme, lang]);

  const refresh = useCallback(async () => {
    try {
      const s = await api.state();
      setState(s);
      if (typeof s.events_head === "number") eventsHead.current = Math.max(eventsHead.current, s.events_head);
    } catch (e) {
      if (e instanceof ApiError && (e.code === "E_UNAUTHORIZED" || e.code === "E_FORBIDDEN")) {
        setSession("missing");
      }
    }
  }, []);

  // Bootstrap: exchange the one-time launcher token (#rcb=…) for a session.
  useEffect(() => {
    (async () => {
      const hash = window.location.hash;
      const m = hash.match(/rcb=([0-9a-fA-F]+)/);
      if (m) {
        try {
          const res = await api.exchange(m[1]);
          setCsrf(res.csrf);
          setSession("ok");
          await refresh();
        } catch {
          // Token may already be used by another tab; fall back to /me.
          const me = await api.me();
          if (me) {
            setCsrf(me.csrf);
            setSession("ok");
            await refresh();
          } else {
            setSession("missing");
          }
        } finally {
          // Clear the fragment immediately (never logged, never re-sent).
          history.replaceState(null, "", window.location.pathname);
        }
      } else {
        const me = await api.me();
        if (me) {
          setCsrf(me.csrf);
          setSession("ok");
          await refresh();
        } else {
          setSession("missing");
        }
      }
    })();
  }, [refresh]);

  // SSE: one connection, DB replay covers gaps; reconnect on drop.
  useEffect(() => {
    if (session !== "ok") return;
    let es: EventSource | null = null;
    let stopped = false;
    let retry: number | undefined;
    const connect = () => {
      if (stopped) return;
      es = new EventSource(api.eventsUrl(eventsHead.current));
      const onEvent = (ev: MessageEvent) => {
        const seq = Number((ev as MessageEvent & { lastEventId?: string }).lastEventId || "0");
        if (seq > eventsHead.current) eventsHead.current = seq;
        refresh();
      };
      es.onmessage = onEvent;
      const bump = () => setDetailTick((x) => x + 1);
      // RC05: every event also refreshes the currently open task detail.
      const esRef = es;
      ["task.created","task.updated","task.captured","task.checked","task.handoff_exported",
       "task.imported","task.accepted","task.candidate_confirmed","project.claimed","check.step",
       "task.profile_saved","operation.done"].forEach((k) => esRef?.addEventListener(k, bump));
      es.addEventListener("task.created", onEvent);
      es.addEventListener("task.updated", onEvent);
      es.addEventListener("task.captured", onEvent);
      es.addEventListener("task.checked", onEvent);
      es.addEventListener("task.handoff_exported", onEvent);
      es.addEventListener("task.imported", onEvent);
      es.addEventListener("task.accepted", onEvent);
      es.addEventListener("task.candidate_confirmed", onEvent);
      es.addEventListener("project.claimed", onEvent);
      es.addEventListener("check.step", (ev) => {
        // step-level updates: cheap refresh for the checking state
        onEvent(ev);
      });
      es.onerror = () => {
        es?.close();
        if (!stopped) retry = window.setTimeout(connect, 2000);
      };
    };
    connect();
    return () => {
      stopped = true;
      if (retry) window.clearTimeout(retry);
      es?.close();
    };
  }, [session, refresh]);

  if (session === "pending") {
    return <div className="empty-state"><p className="sub">{t.loading}</p></div>;
  }
  if (session === "missing") {
    return (
      <div className="empty-state">
        <div className="brand" style={{ justifyContent: "center" }}>
          <BuckleMark className="mark" />
          RelayCheck
        </div>
        <h1>{t.errorTitle}</h1>
        <p className="sub">{t.sessionMissing}</p>
      </div>
    );
  }

  const tasks: any[] = state?.tasks ?? [];
  const selected = tasks.find((x) => x.id === route.taskId) ?? null;

  const nav = (
    <nav className="nav" aria-label={t.handoffs}>
      {([
        ["handoffs", "↗", t.handoffs],
        ["tasks", "▤", t.tasks],
        ["settings", "⚙", t.settings],
      ] as const).map(([view, sym, label]) => (
        <button
          key={view}
          className="nav-item"
          aria-current={route.view === view}
          onClick={() => setRoute({ view, taskId: route.view === view ? route.taskId : null })}
        >
          <span className="nav-symbol" aria-hidden>{sym}</span>
          <span>{label}</span>
        </button>
      ))}
    </nav>
  );

  const claimProject = async (code: string) => {
    await api.claim(code.trim());
    await refresh();
  };

  const createDemo = async () => {
    await api.demo();
    await refresh();
  };

  const listItems = (tasksList: any[]) =>
    tasksList.map((task) => (
      <button
        key={task.id}
        className="task-card"
        aria-current={route.taskId === task.id}
        onClick={() => setRoute({ view: route.view, taskId: task.id })}
      >
        <div className="eyebrow">{task.project_name}</div>
        <strong>{task.goal}</strong>
        <div className="sub">
          <StatusDot task={task} />
          <StatusText task={task} t={t} />
        </div>
        {task.is_demo ? (
          <span className="badge neutral" style={{ marginTop: 10 }}>{t.demoBadge}</span>
        ) : null}
      </button>
    ));

  const visibleTasks = route.view === "tasks" ? tasks : tasks;

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand">
          <BuckleMark className="mark" />
          RelayCheck
        </div>
        {nav}
        <div className="side-note">
          <span className="local-dot" aria-hidden /> {t.localNote}
          <div style={{ marginTop: 7 }}>{t.noAuto}</div>
        </div>
      </aside>

      <section className="inbox">
        <h2>{route.view === "tasks" ? t.tasks : t.handoffs}</h2>
        <p className="sub">{t.inboxSub}</p>
        {visibleTasks.length === 0 ? null : listItems(visibleTasks)}
        <NewTaskInline
          t={t}
          lang={lang}
          projects={state?.projects ?? []}
          onDemo={createDemo}
          onCreated={async (taskId) => {
            await refresh();
            setRoute({ view: route.view, taskId });
          }}
        />
      </section>

      <main className="detail">
        <div className="detail-top">
          <div className="sub">
            <button
              className="link-btn mobile-only"
              onClick={() => setRoute({ view: route.view, taskId: null })}
            >
              ‹ {t.back}
            </button>
            {t.breadcrumb}
          </div>
          <div className="controls">
            <button className="link-btn" onClick={() => setLang(lang === "zh" ? "en" : "zh")} aria-label="language">
              {lang === "zh" ? "EN" : "中文"}
            </button>
            <button
              className="link-btn"
              onClick={() => setTheme(document.documentElement.dataset.theme === "dark" ? "light" : "dark")}
              aria-label="theme"
            >
              {document.documentElement.dataset.theme === "dark" ? t.light : t.dark}
            </button>
            <button className="link-btn" onClick={() => setRoute({ view: "settings", taskId: null })}>
              {t.settings}
            </button>
          </div>
        </div>

        {route.view === "settings" ? (
          <SettingsPanel t={t} lang={lang} theme={theme} setLang={setLang} setTheme={setTheme} />
        ) : selected ? (
          <TaskDetail
            key={selected.id}
            taskId={selected.id}
            t={t}
            lang={lang}
            onBack={() => setRoute({ view: route.view, taskId: null })}
            onChanged={refresh}
            detailTick={detailTick}
          />
        ) : (
          <EmptyState t={t} lang={lang} projects={state?.projects ?? []} onClaim={claimProject} onDemo={createDemo} />
        )}
      </main>
    </div>
  );
}

function StatusDot({ task }: { task: any }) {
  const s = task.workflow_state;
  if (s === "ready_for_review") return <span className="dot ok" aria-hidden />;
  if (s === "draft") return <span className="dot muted" aria-hidden />;
  if (s === "accepted") return <span className="dot ok" aria-hidden />;
  return <span className="dot" aria-hidden />;
}

function StatusText({ task, t }: { task: any; t: (typeof dict)["zh"] }) {
  const s: string = task.workflow_state;
  if (s === "draft") return <>{t.draft}</>;
  if (s === "candidate") return <>{t.candidate} · {t.externalUnobserved}</>;
  if (s === "checking") return <>{t.checking}</>;
  if (s === "ready_for_review") return <>{t.passedOn} · {t.verifyReturn}</>;
  if (s === "accepted") return <>{t.accepted}</>;
  if (s === "rework") return <>{t.viewIssues}</>;
  return <>{s}</>;
}

function EmptyState({
  t, lang, projects, onClaim, onDemo,
}: {
  t: (typeof dict)["zh"];
  lang: Lang;
  projects: any[];
  onClaim: (code: string) => Promise<void>;
  onDemo: () => Promise<void>;
}) {
  const [code, setCode] = useState("");
  const [err, setErr] = useState<string | null>(null);
  return (
    <div className="empty-state">
      <div className="eyebrow">{t.taskHandoff}</div>
      <h1>{t.emptyTitle}</h1>
      <p className="sub">{t.emptyBody}</p>
      <ol className="steps-list">
        <li>
          <code className="mono-block">relaycheck attach D:\path\to\your-repo</code>
        </li>
      </ol>
      <div className="inline-form">
        <input
          type="text"
          value={code}
          onChange={(e) => setCode(e.target.value)}
          placeholder={`${t.claimLabel} (a1b2c3d4…)`}
          aria-label={t.claimLabel}
        />
        <button
          className="primary"
          onClick={async () => {
            setErr(null);
            try {
              await onClaim(code);
            } catch (e) {
              setErr(String((e as ApiError).message ?? e));
            }
          }}
        >
          {t.claimBtn}
        </button>
      </div>
      {err ? <div className="error-note" role="alert">{err}</div> : null}
      {projects.length > 0 ? (
        <p className="sub">{projects.length} {t.projectCount}</p>
      ) : null}
      <div className="actions">
        <button
          className="secondary"
          onClick={async () => {
            setErr(null);
            try {
              await onDemo();
            } catch (e) {
              setErr(String((e as ApiError).message ?? e));
            }
          }}
        >
          {t.tryDemo}
        </button>
      </div>
      <p className="small-note">{t.demoNote}</p>
      <p className="small-note">{lang === "zh" ? t.storedLocally : t.storedLocally}</p>
    </div>
  );
}

function NewTaskInline({
  t, projects, onDemo, onCreated,
}: {
  t: (typeof dict)["zh"];
  lang: Lang;
  projects: any[];
  onDemo: () => Promise<void>;
  onCreated: (taskId: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [goal, setGoal] = useState("");
  const [projectId, setProjectId] = useState(projects[0]?.id ?? "");
  const [err, setErr] = useState<string | null>(null);
  useEffect(() => {
    if (!projectId && projects[0]) setProjectId(projects[0].id);
  }, [projects, projectId]);
  if (projects.length === 0) {
    return (
      <div className="actions" style={{ marginTop: 12 }}>
        <button className="ghost" onClick={() => onDemo().catch(() => undefined)}>{t.tryDemo}</button>
      </div>
    );
  }
  if (!open) {
    return (
      <div className="actions" style={{ marginTop: 12 }}>
        <button className="primary" onClick={() => setOpen(true)}>{t.newTaskBtn}</button>
      </div>
    );
  }
  return (
    <div style={{ marginTop: 12 }}>
      <label className="field">
        <span>{t.goalLabel}</span>
        <textarea value={goal} onChange={(e) => setGoal(e.target.value)} placeholder={t.goalPlaceholder} />
      </label>
      <label className="field">
        <span>project</span>
        <select value={projectId} onChange={(e) => setProjectId(e.target.value)}>
          {projects.map((p) => (
            <option key={p.id} value={p.id}>{p.display_name}</option>
          ))}
        </select>
      </label>
      {err ? <div className="error-note" role="alert">{err}</div> : null}
      <div className="actions">
        <button
          className="primary"
          onClick={async () => {
            setErr(null);
            try {
              const r = await api.createTask(
                { project_id: projectId, goal, constraints: [], criteria: [] },
                crypto.randomUUID(),
              );
              setOpen(false);
              setGoal("");
              onCreated(r.result.task_id);
            } catch (e) {
              setErr(String((e as ApiError).message ?? e));
            }
          }}
        >
          {t.createTask}
        </button>
        <button className="secondary" onClick={() => setOpen(false)}>{t.cancel}</button>
      </div>
    </div>
  );
}

function SettingsPanel({
  t, lang, theme, setLang, setTheme,
}: {
  t: (typeof dict)["zh"];
  lang: Lang;
  theme: Theme;
  setLang: (l: Lang) => void;
  setTheme: (th: Theme) => void;
}) {
  return (
    <div className="card" style={{ maxWidth: 640 }}>
      <h2>{t.settingsTitle}</h2>
      <label className="field">
        <span>{t.language}</span>
        <select value={lang} onChange={(e) => setLang(e.target.value as Lang)}>
          <option value="zh">中文</option>
          <option value="en">English</option>
        </select>
      </label>
      <label className="field">
        <span>{t.theme}</span>
        <select value={theme} onChange={(e) => setTheme(e.target.value as Theme)}>
          <option value="system">{t.themeAuto}</option>
          <option value="light">{t.light}</option>
          <option value="dark">{t.dark}</option>
        </select>
      </label>
      <p className="small-note">{t.aboutNote}</p>
    </div>
  );
}
