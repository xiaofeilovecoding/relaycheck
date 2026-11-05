import { useCallback, useEffect, useRef, useState } from "react";
import { api, ApiError } from "./api";
import { dict, Lang } from "./i18n";
import { ErrorNote, Modal } from "./components";

type T = (typeof dict)["zh"];

export function TaskDetail({
  taskId, t, lang, onBack, onChanged,
}: {
  taskId: string;
  t: T;
  lang: Lang;
  onBack: () => void;
  onChanged: () => Promise<void>;
}) {
  const [detail, setDetail] = useState<any>(null);
  const [error, setError] = useState<ApiError | null>(null);
  const [opError, setOpError] = useState<ApiError | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [modal, setModal] = useState<"capture" | "handoff" | "import" | "profile" | null>(null);
  const [flash, setFlash] = useState(false);
  const [activeOpId, setActiveOpId] = useState<string | null>(null);
  const [criteriaAck, setCriteriaAck] = useState<string[]>([]);
  const lastSnapshot = useRef<string | null>(null);

  const load = useCallback(async () => {
    try {
      const r = await api.task(taskId);
      setDetail(r.result);
      setError(null);
    } catch (e) {
      setError(e as ApiError);
    }
  }, [taskId]);

  useEffect(() => {
    load();
  }, [load]);

  useEffect(() => {
    const cs: any[] = detail?.task?.criteria ?? [];
    setCriteriaAck(cs.filter((c: any) => c.required).map((c: any) => c.id));
  }, [detail?.task?.id, detail?.task?.revision]);

  // While a check round is running, poll the detail so the UI follows the
  // real state machine (SSE handles the task list; detail reloads here).
  useEffect(() => {
    if (detail?.task?.workflow_state !== "checking") return;
    const h = window.setInterval(() => { load(); }, 2000);
    return () => window.clearInterval(h);
  }, [detail?.task?.workflow_state, load]);

  // One-shot candidate alignment (≤160ms) whenever the active candidate
  // changes; idle renders stay static (CSS also honors reduced-motion).
  useEffect(() => {
    const active = detail?.task?.active_snapshot ?? null;
    if (active && lastSnapshot.current && active !== lastSnapshot.current) {
      setFlash(true);
      const h = window.setTimeout(() => setFlash(false), 200);
      return () => window.clearTimeout(h);
    }
    lastSnapshot.current = active;
  }, [detail?.task?.active_snapshot]);

  const runOp = useCallback(
    async (kind: string, fn: () => Promise<{ operation: any }>) => {
      setOpError(null);
      setBusy(kind);
      try {
        const r = await fn();
        const opId = r.operation?.id;
        if (!opId) return;
        setActiveOpId(opId);
        for (;;) {
          await new Promise((res) => setTimeout(res, 500));
          const op = await api.operation(opId);
          if (op.state === "completed") break;
          if (op.state === "failed" || op.state === "cancelled" || op.state === "interrupted") {
            const err = op.error ?? {};
            setOpError(new ApiError(err.code ?? op.state, err.message ?? "operation failed", err.details ?? []));
            break;
          }
        }
        await load();
        await onChanged();
      } catch (e) {
        setOpError(e as ApiError);
      } finally {
        setBusy(null);
      }
    },
    [load, onChanged],
  );

  if (error) {
    return (
      <div>
        <p className="sub">{t.errorTitle}: {error.message}</p>
        <button className="secondary" onClick={onBack}>{t.back}</button>
      </div>
    );
  }
  if (!detail) {
    return <p className="sub">{t.loading}</p>;
  }

  const task = detail.task;
  const snapshots: any[] = detail.snapshots ?? [];
  const verifications: any[] = detail.verifications ?? [];
  const handoffs: any[] = detail.handoffs ?? [];
  const imported: any[] = detail.imported_reports ?? [];
  const acceptances: any[] = detail.acceptances ?? [];
  const profile = detail.profile?.profile ?? null;
  const profileTrusted = detail.profile?.trusted ?? false;
  const acceptance = detail.acceptance ?? { ok: false, missing: [] };
  const primary = detail.primary ?? { action: null, reason: "" };
  const diff = detail.diff ?? null;
  const active = task.active_snapshot ?? null;
  const activeSnap = snapshots.find((s) => s.id === active);
  const checking = task.workflow_state === "checking";
  const runningOp = busy === "capture" || busy === "verify" || checking;
  const lastExport = handoffs[handoffs.length - 1] ?? null;

  // The latest passing verification that does NOT belong to the active
  // candidate → the stale banner story.
  const staleVerifiedElsewhere = detail.stale_banner === true;

  const parentHandoffId: string | null = task.parent_handoff ?? null;

  const criteria: any[] = task.criteria ?? [];

  return (
    <div>
      <div className="eyebrow">{t.taskHandoff}</div>
      <h1 className="goal">{task.goal}</h1>
      <div className="metadata">
        {active ? (
          <span className={`hash-chip candidate ${flash ? "align-once" : ""}`} title={active}>
            <code>{short(active)}{isReturnOf(parentHandoffId, handoffs) ? " · B" : " · A"}</code>
          </span>
        ) : (
          <span className="hash-chip"><code>{t.noSnapshot}</code></span>
        )}
        {activeSnap ? (
          <span>
            {t.fromA} {activeSnap.base_head ? short(activeSnap.base_head) : "—"}
            {activeSnap.dirty ? " · dirty" : ""}
          </span>
        ) : null}
        <span>·</span>
        <span>{t.scopeSelected}</span>
        <span>·</span>
        <span>{t.externalUnobserved}</span>
      </div>

      <div className="rail" aria-label="progress">
        {steps(task, handoffs, acceptances, t).map((s, i) => (
          <span key={i} style={{ display: "contents" }}>
            {i > 0 ? <span className="sep" aria-hidden /> : null}
            <span className={s.state}>{s.label}</span>
          </span>
        ))}
      </div>

      {task.is_demo ? (
        <div className="banner" role="status">
          <span className="bang">Demo</span>
          <div>
            <strong>{t.demoBadge}</strong>
            <span>{lang === "zh" ? "样例仓库在本机创建，检查为本机真实执行，不联网。" : "The sample repository is local; checks really run here, offline."}</span>
          </div>
        </div>
      ) : null}

      {task.workflow_state === "accepted" ? (
        <div className="banner" role="status">
          <span className="bang">✓</span>
          <div>
            <strong>{t.accepted}</strong>
            <span>{acceptances.length > 0 ? `${t.candidate}: ${short(acceptances[acceptances.length - 1].snapshot_id)}` : ""}</span>
          </div>
        </div>
      ) : acceptances.length > 0 && task.workflow_state !== "accepted" ? (
        <div className="banner" role="status">
          <span className="bang">i</span>
          <div>
            <strong>{t.historyAccepted}</strong>
            <span>{t.thisPassesOld}</span>
          </div>
        </div>
      ) : staleVerifiedElsewhere ? (
        <div className="banner" role="status">
          <span className="bang">!</span>
          <div>
            <strong>{t.staleTitle}</strong>
            <span>{t.staleBody}</span>
          </div>
        </div>
      ) : null}

      <ErrorNote error={opError?.message} details={opError?.details} />

      <div className="split">
        <section className="card">
          <div className="kicker">{t.nextKicker}</div>
          <NextActionLine detail={detail} lang={lang} />
          {task.constraints?.length ? (
            <>
              <div className="rule" />
              <div className="eyebrow">{t.constraints}</div>
              <ul className="constraint-list">
                {task.constraints.map((c: string, i: number) => (
                  <li key={i}>{c}</li>
                ))}
              </ul>
            </>
          ) : null}
          {(lastExport?.body?.open_items ?? []).length > 0 ? (
            <>
              <div className="rule" />
              <div className="eyebrow">{t.stillOpen}</div>
              {lastExport.body.open_items.map((o: any) => (
                <div className="open-item" key={o.id}>
                  <span className="num">{o.id}</span>
                  <span>{o.text}</span>
                </div>
              ))}
            </>
          ) : null}

          <div className="actions">
            {primary.action === "capture" ? (
              <button className="primary" onClick={() => setModal("capture")} disabled={runningOp}>
                {t.captureCandidate}
              </button>
            ) : null}
            {primary.action === "run_checks" ? (
              profile ? (
                <button
                  className="primary"
                  disabled={runningOp}
                  onClick={() =>
                    runOp("verify", () =>
                      api.verify(task.id, true, crypto.randomUUID()),
                    )
                  }
                >
                  {checking ? t.checking : t.runChecks}
                </button>
              ) : (
                <button className="primary" onClick={() => setModal("profile")} disabled={runningOp}>
                  {t.runChecks}
                </button>
              )
            ) : null}
            {primary.action === "view_issues" ? (
              <button className="primary" onClick={() => setModal("profile")} disabled={runningOp}>
                {t.viewIssues}
              </button>
            ) : null}
            {primary.action === "prepare_handoff" ? (
              <button className="primary" onClick={() => setModal("handoff")}>
                {t.prepareHandoff}
              </button>
            ) : null}
            {primary.action === "accept" ? null : null}
            {active && primary.action !== "capture" ? (
              <button className="secondary" onClick={() => setModal("capture")} disabled={runningOp}>
                {t.captureCandidate}
              </button>
            ) : null}
            {parentHandoffId ? (
              <button className="secondary" onClick={() => setModal("import")} disabled={runningOp}>
                {t.importReturn}
              </button>
            ) : null}
            {checking ? <CancelButton opId={activeOpId ?? ""} onDone={() => { load(); onChanged(); }} /> : null}
          </div>
          {primary.action ? <p className="small-note">{primary.reason}</p> : null}
          <p className="small-note">{t.checksRunLocal}</p>
        </section>

        <section>
          <div className="card">
            <h2>{t.checksTitle}</h2>
            <p className="sub" style={{ margin: 0 }}>{t.checksSub}</p>
            {profile ? (
              <div className="check">
                <div className="name">
                  <code>{profile.name}</code>
                  {!profileTrusted ? <span className="badge neutral">unapproved</span> : null}
                </div>
                <div className="sub mono-block">{describeProfile(profile)}</div>
              </div>
            ) : null}
            {latestRound(verifications).map((r: any) => (
              <div className="check" key={r.check_id}>
                <div className="name">
                  <code>{r.check_id}</code>
                  <StatusBadge r={r} t={t} />
                </div>
                <div className="sub">
                  {r.status === "passed" ? "" : ""}
                  {r.status === "failed" ? `exit ${r.exit_code ?? "?"}` : ""}
                  {r.status === "blocked" ? r.detail || t.notRun : ""}
                  {r.status === "invalidated" ? r.detail : ""}
                  {r.status === "interrupted" ? r.detail || "interrupted" : ""}
                  {r.truncated ? " · truncated" : ""}
                  {r.redactions > 0 ? ` · ${r.redactions} redaction(s)` : ""}
                </div>
              </div>
            ))}
            {imported.length > 0 ? (
              <div className="check">
                <div className="name">
                  <span>{t.returnedReport}</span>
                  <span className="badge neutral">{t.externalClaim}</span>
                </div>
                <div className="sub">
                  {describeClaims(imported[imported.length - 1])} — {t.needsLocalRerun}
                </div>
              </div>
            ) : null}
            <div className="rule" />
            <div className="scope-row">
              <span>{t.included}</span>
              <strong>{activeSnap ? `${activeSnap.file_count} ${t.files}` : "—"}</strong>
            </div>
            <div className="scope-row">
              <span>{t.excluded}</span>
              <strong>{lang === "zh" ? "秘密、依赖目录" : "secrets, dependency dirs"}</strong>
            </div>
          </div>

          <div className="accept-area">
            <button
              className="primary"
              disabled={!acceptance.ok || task.workflow_state === "checking" || runningOp}
              onClick={() => {
                setCriteriaAck(criteria.map((c: any) => c.id));
                runOp("accept", () =>
                  api.accept(task.id, criteriaAck, task.revision, crypto.randomUUID()),
                );
              }}
            >
              {t.acceptBtn}
            </button>
            {!acceptance.ok && acceptance.missing?.length > 0 ? (
              <p>
                {t.acceptReasonPrefix}
                <ul style={{ margin: "4px 0 0 18px", padding: 0 }}>
                  {acceptance.missing.slice(0, 5).map((m: string, i: number) => (
                    <li key={i}>{m}</li>
                  ))}
                </ul>
              </p>
            ) : (
              <p>{lang === "zh" ? "不会自动应用或合并改动。" : "Changes are not applied or merged automatically."}</p>
            )}
          </div>
        </section>
      </div>

      {diff && diff.baseline ? (
        <section className="diff-area">
          <div className="section-head">
            <h2>{t.whatCameBack}</h2>
            <span className="sub">
              {short(diff.baseline)} → {short(diff.candidate)} · {diff.files?.length ?? 0} {t.files}
            </span>
          </div>
          {diff.baseline_available === false ? (
            <div className="banner" role="status">
              <span className="bang">i</span>
              <div><span>{t.baseUnavailable}</span></div>
            </div>
          ) : (
            <div className="file-table">
              {(diff.files ?? []).map((f: any) => (
                <DiffRow key={f.path} f={f} t={t} />
              ))}
              {(diff.files ?? []).length === 0 ? (
                <div className="file-row"><code>—</code><span className="stat">{lang === "zh" ? "内容相同" : "no content change"}</span></div>
              ) : null}
            </div>
          )}
        </section>
      ) : null}

      <details className="activity">
        <summary>{t.activity}</summary>
        <ActivityList detail={detail} />
      </details>

      {modal === "capture" ? (
        <CaptureModal
          t={t}
          taskId={task.id}
          onClose={() => setModal(null)}
          onDone={async () => {
            setModal(null);
            await load();
            await onChanged();
          }}
        />
      ) : null}
      {modal === "handoff" ? (
        <HandoffModal
          t={t}
          lang={lang}
          taskId={task.id}
          task={task}
          onClose={() => setModal(null)}
          onDone={async () => {
            setModal(null);
            await load();
            await onChanged();
          }}
        />
      ) : null}
      {modal === "import" ? (
        <ImportModal
          t={t}
          taskId={task.id}
          onClose={() => setModal(null)}
          onDone={async (snapshotId: string | null) => {
            setModal(null);
            if (snapshotId) {
              const rev = (await api.task(task.id)).result.task.revision;
              await api.confirmCandidate(task.id, snapshotId, rev).catch((e) => setOpError(e));
            }
            await load();
            await onChanged();
          }}
        />
      ) : null}
      {modal === "profile" ? (
        <ProfileModal
          t={t}
          taskId={task.id}
          profile={profile}
          onClose={() => setModal(null)}
          onDone={async () => {
            setModal(null);
            await runOp("verify", () => api.verify(task.id, true, crypto.randomUUID()));
          }}
        />
      ) : null}
    </div>
  );
}


function isReturnOf(parentHandoffId: string | null, handoffs: any[]): boolean {
  return !!parentHandoffId && handoffs.some((h) => h.id === parentHandoffId);
}

function steps(task: any, handoffs: any[], acceptances: any[], t: T) {
  const out: { label: string; state: string }[] = [];
  out.push({ label: `✓ ${t.captured}`, state: task.active_snapshot ? "done" : "" });
  out.push({ label: `✓ ${t.handedOff}`, state: handoffs.length > 0 ? "done" : "" });
  out.push({ label: `↳ ${t.returned}`, state: handoffs.length > 0 && task.active_snapshot ? "now" : "" });
  out.push({ label: `○ ${t.accept}`, state: acceptances.length > 0 ? "done" : "" });
  return out;
}

function short(id: string): string {
  const bare = id?.startsWith("sha256:") ? id.slice(7) : id ?? "";
  return bare.slice(0, 7);
}

function describeProfile(profile: any): string {
  const checks = (profile.checks ?? [])
    .map((c: any) => `${c.exe} ${(c.args ?? []).join(" ")}`)
    .join("\n");
  return checks || "—";
}

function describeClaims(report: any): string {
  const claims = report?.claims ?? [];
  if (!Array.isArray(claims) || claims.length === 0) return "—";
  return claims.map((c: any) => `${c.check_id}: ${c.status}`).join(", ");
}

function latestRound(verifications: any[]): any[] {
  if (verifications.length === 0) return [];
  const v = verifications[0]; // server returns DESC by created time
  return v.results ?? [];
}

function StatusBadge({ r, t }: { r: any; t: T }) {
  if (r.status === "passed") return <span className="badge ok">{t.passedOn}</span>;
  if (r.status === "failed") return <span className="badge bad">failed</span>;
  if (r.status === "blocked") return <span className="badge bad">blocked</span>;
  if (r.status === "invalidated") return <span className="badge bad">invalidated</span>;
  if (r.status === "interrupted") return <span className="badge neutral">interrupted</span>;
  return <span className="badge neutral">{t.notRun}</span>;
}

function NextActionLine({ detail, lang }: { detail: any; lang: Lang }) {
  const handoffs: any[] = detail.handoffs ?? [];
  const next = handoffs.length > 0 ? handoffs[handoffs.length - 1]?.body?.next_action : null;
  if (next) {
    return <p className="next-action">{next}</p>;
  }
  const primary = detail.primary?.action;
  const fallback =
    lang === "zh"
      ? { capture: "先捕获一个候选快照，把当前工作定格。", run_checks: "先运行本机检查，确认当前候选真实有效。", view_issues: "查看失败的检查，处理问题后再复验。", prepare_handoff: "准备接手卡，把下一步交给下一个工具。", accept: "全部必选检查通过，可以接受当前候选。" }[primary as string] ?? "—"
      : { capture: "Capture a candidate snapshot to pin the current work.", run_checks: "Run the local checks to verify the current candidate.", view_issues: "Look at the failing checks, fix, then re-verify.", prepare_handoff: "Prepare the handoff card for the next tool.", accept: "All required checks passed — the candidate can be accepted." }[primary as string] ?? "—";
  return <p className="next-action">{fallback}</p>;
}

function DiffRow({ f, t }: { f: any; t: T }) {
  const [open, setOpen] = useState(false);
  return (
    <div>
      <div className="file-row" title={f.kind === "deleted" ? t.deleted : f.kind === "added" ? t.added : f.binary ? t.binary : t.modified}>
        <code>{f.path}</code>
        <span className="stat">
          {f.kind === "deleted" ? (
            <b className="minus">{t.deleted}</b>
          ) : f.binary ? (
            <span>{f.old_size ?? "?"} → {f.new_size ?? "?"} B</span>
          ) : (
            <>
              <b className="plus">+{f.plus}</b> <b className="minus">−{f.minus}</b>
            </>
          )}
        </span>
      </div>
      {f.unified && f.unified.length > 0 ? (
        <div style={{ padding: "0 16px 12px" }}>
          <button className="ghost" onClick={() => setOpen(!open)} aria-expanded={open}>
            {open ? "▾" : "▸"} diff
          </button>
          {open ? (
            <div className="diff-unified" tabIndex={0}>
              {f.unified.map((line: string, i: number) => (
                <div key={i} className={line.startsWith("+") ? "add" : line.startsWith("-") ? "del" : ""}>
                  {line}
                </div>
              ))}
              {f.truncated ? <div>…</div> : null}
            </div>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

function ActivityList({ detail }: { detail: any }) {
  const items: string[] = [];
  for (const v of detail.verifications ?? []) {
    items.push(`check round ${v.round_status} · ${short(v.snapshot_id)}`);
  }
  for (const h of detail.handoffs ?? []) {
    items.push(`handoff exported (${h.source_mode}) · ${h.id.slice(0, 8)}`);
  }
  for (const r of detail.imported_reports ?? []) {
    items.push(`import · ${r.origin} · ${r.association}`);
  }
  for (const a of detail.acceptances ?? []) {
    items.push(`accepted ${short(a.snapshot_id)} · rev ${a.task_revision}`);
  }
  return (
    <ul>
      {items.length === 0 ? <li>—</li> : items.map((x, i) => <li key={i}>{x}</li>)}
    </ul>
  );
}

function CancelButton({ opId, onDone }: { opId: string; onDone: () => void }) {
  return (
    <button
      className="secondary"
      onClick={async () => {
        await api.cancelOperation(opId).catch(() => undefined);
        onDone();
      }}
    >
      Stop
    </button>
  );
}

// ---------------------------------------------------------------------------
// Modals
// ---------------------------------------------------------------------------

function CaptureModal({
  t, taskId, onClose, onDone,
}: {
  t: T;
  taskId: string;
  onClose: () => void;
  onDone: () => Promise<void>;
}) {
  const [plan, setPlan] = useState<any>(null);
  const [untracked, setUntracked] = useState<string[]>([]);
  const [attest, setAttest] = useState(false);
  const [err, setErr] = useState<ApiError | null>(null);
  const [busy, setBusy] = useState(false);
  const firstRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    (async () => {
      try {
        const p = await api.capturePlan(taskId, { untracked_selected: [], extra_excludes: [] });
        setPlan(p.result);
      } catch (e) {
        setErr(e as ApiError);
      }
    })();
  }, [taskId]);

  const refreshPlan = async (selected: string[]) => {
    try {
      const p = await api.capturePlan(taskId, { untracked_selected: selected, extra_excludes: [] });
      setPlan(p.result);
    } catch (e) {
      setErr(e as ApiError);
    }
  };

  return (
    <Modal title={t.captureConfirmTitle} onClose={onClose} initialFocusRef={firstRef}>
      <h2>{t.captureConfirmTitle}</h2>
      <p className="sub">{t.externalStopConfirm}</p>
      <ErrorNote error={err?.message} details={err?.details} />
      {plan ? (
        <>
          <div className="eyebrow" style={{ marginTop: 10 }}>
            {t.captureScope} ({plan.included.length})
          </div>
          <div className="mono-block" style={{ maxHeight: 180, overflowY: "auto" }}>
            {plan.included.map((f: string) => (
              <div key={f}>{f}</div>
            ))}
          </div>
          {plan.untracked && plan.untracked.length > 0 ? (
            <>
              <div className="eyebrow" style={{ marginTop: 12 }}>{t.untrackedTitle}</div>
              {plan.untracked.map((f: string) => (
                <label className="checkline" key={f}>
                  <input
                    type="checkbox"
                    checked={untracked.includes(f)}
                    onChange={(e) => {
                      const next = e.target.checked
                        ? [...untracked, f]
                        : untracked.filter((x) => x !== f);
                      setUntracked(next);
                      refreshPlan(next);
                    }}
                  />
                  <span className="mono-block" style={{ padding: 0, border: 0, background: "transparent" }}>{f}</span>
                </label>
              ))}
              <button className="ghost" onClick={() => { const all = plan.untracked as string[]; setUntracked(all); refreshPlan(all); }}>
                {t.selectAll}
              </button>
            </>
          ) : null}
          {plan.excluded?.length > 0 ? (
            <>
              <div className="eyebrow" style={{ marginTop: 12 }}>{t.captureExcluded}</div>
              <div className="mono-block" style={{ maxHeight: 120, overflowY: "auto" }}>
                {plan.excluded.map((x: any) => (
                  <div key={x.path}>{x.path} — {x.reason}</div>
                ))}
              </div>
            </>
          ) : null}
          <label className="checkline">
            <input type="checkbox" checked={attest} onChange={(e) => setAttest(e.target.checked)} />
            <span>{t.captureAttest}</span>
          </label>
          <div className="actions">
            <button
              ref={firstRef as any}
              className="primary"
              disabled={!attest || busy}
              onClick={async () => {
                setBusy(true);
                setErr(null);
                try {
                  const r = await api.capture(taskId, { attest_stopped: true, untracked_selected: untracked, extra_excludes: [] }, crypto.randomUUID());
                  // Wait for the operation to settle before refreshing: the
                  // SSE event alone does not reload this detail view.
                  const opId = r?.operation?.id;
                  if (opId) {
                    for (;;) {
                      await new Promise((res) => setTimeout(res, 500));
                      const op = await api.operation(opId);
                      const st = op.state as string;
                      if (st === "completed") break;
                      if (st === "failed" || st === "cancelled" || st === "interrupted") {
                        const err = op.error ?? {};
                        throw new ApiError(err.code ?? st, err.message ?? "capture failed", err.details ?? []);
                      }
                    }
                  }
                  await onDone();
                } catch (e) {
                  setErr(e as ApiError);
                  setBusy(false);
                }
              }}
            >
              {t.captureBtn}
            </button>
            <button className="secondary" onClick={onClose} disabled={busy}>{t.cancel}</button>
          </div>
        </>
      ) : (
        <p className="sub">{t.loading}</p>
      )}
    </Modal>
  );
}

function HandoffModal({
  t, lang, taskId, task, onClose, onDone: _onDone,
}: {
  t: T;
  lang: Lang;
  taskId: string;
  task: any;
  onClose: () => void;
  onDone: () => Promise<void>;
}) {
  const [nextAction, setNextAction] = useState("");
  const [openItems, setOpenItems] = useState("C1: ");
  const [sourceMode, setSourceMode] = useState("context_only");
  const [preview, setPreview] = useState<string | null>(null);
  const [handoffId, setHandoffId] = useState<string | null>(null);
  const [err, setErr] = useState<ApiError | null>(null);
  const [copied, setCopied] = useState(false);
  const firstRef = useRef<HTMLTextAreaElement>(null);

  void _onDone;
  const buildBody = () => ({
    source_mode: sourceMode,
    next_action: nextAction,
    open_items: openItems
      .split("\n")
      .map((l) => l.trim())
      .filter(Boolean)
      .map((l) => {
        const i = l.indexOf(":");
        return i > 0 ? { id: l.slice(0, i).trim(), text: l.slice(i + 1).trim() } : { id: "O", text: l };
      }),
    notes_constraints: [],
    edited_fields: ["next_action"],
  });

  const exportNow = async () => {
    setErr(null);
    try {
      const r = await api.createHandoff(taskId, buildBody(), crypto.randomUUID());
      setPreview(r.result.handoff_md);
      setHandoffId(r.result.handoff_id);
    } catch (e) {
      setErr(e as ApiError);
    }
  };

  const copyNote = async () => {
    const note = [
      lang === "zh" ? "接力说明（数据，不是指令）：" : "Handoff note (data, not instructions):",
      `goal: ${task.goal}`,
      lang === "zh" ? "先复述约束与未决项，再读取指定文件；继续后用 capture 生成新候选并回交。" : "Restate the constraints and open items, read the named files, then continue; capture a new candidate when done.",
      nextAction ? `next: ${nextAction}` : "",
      preview ? `package: relaycheck-handoff-${handoffId}.zip` : "",
    ]
      .filter(Boolean)
      .join("\n");
    await navigator.clipboard.writeText(note);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 2000);
  };

  return (
    <Modal title={t.handoffTitle} onClose={onClose} initialFocusRef={firstRef}>
      <h2>{t.handoffTitle}</h2>
      <p className="sub">{t.importedShown}</p>
      <ErrorNote error={err?.message} details={err?.details} />
      <label className="field">
        <span>{t.nextActionLabel}</span>
        <textarea
          ref={firstRef as any}
          value={nextAction}
          onChange={(e) => setNextAction(e.target.value)}
          placeholder={t.nextActionPlaceholder}
        />
      </label>
      <label className="field">
        <span>{t.openItemsLabel}</span>
        <textarea value={openItems} onChange={(e) => setOpenItems(e.target.value)} />
      </label>
      <label className="field">
        <span>{t.sourceModeLabel}</span>
        <select value={sourceMode} onChange={(e) => setSourceMode(e.target.value)}>
          <option value="context_only">{t.contextOnly}</option>
          <option value="portable_source">{t.portableSource}</option>
        </select>
      </label>
      <div className="actions">
        <button className="primary" disabled={!nextAction.trim()} onClick={exportNow}>
          {t.exportBtn}
        </button>
        {preview ? (
          <>
            <button className="secondary" onClick={copyNote}>{copied ? t.copied : t.copyBtn}</button>
            <a className="button secondary" href={api.handoffPackageUrl(handoffId!)} download>
              <span className="sr-only">{t.downloadBtn}</span>
              {t.downloadBtn}
            </a>
          </>
        ) : null}
      </div>
      {!nextAction.trim() ? (
        <p className="small-note">{lang === "zh" ? "未填下一步只能保存草稿，不能导出正式接手卡。" : "Without a next action only a draft can be kept — no formal export."}</p>
      ) : null}
      {preview ? (
        <>
          <div className="eyebrow" style={{ marginTop: 14 }}>
            {t.handoffTitle} · {t.handoffSchema}
          </div>
          <div className="handoff-preview">{preview}</div>
          <p className="small-note">{t.exported}</p>
        </>
      ) : null}
    </Modal>
  );
}

function ImportModal({
  t, taskId, onClose, onDone,
}: {
  t: T;
  taskId: string;
  onClose: () => void;
  onDone: (snapshotId: string | null) => Promise<void>;
}) {
  const [err, setErr] = useState<ApiError | null>(null);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<any>(null);
  const fileRef = useRef<HTMLInputElement>(null);

  const pick = async (file: File | undefined) => {
    if (!file) return;
    setBusy(true);
    setErr(null);
    try {
      const r = await api.importPackage(taskId, file);
      setResult(r.result);
    } catch (e) {
      setErr(e as ApiError);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal title={t.importTitle} onClose={onClose} initialFocusRef={fileRef}>
      <h2>{t.importTitle}</h2>
      <p className="sub">{t.importNote}</p>
      <ErrorNote error={err?.message} details={err?.details} />
      <input
        ref={fileRef as any}
        type="file"
        accept=".zip,application/zip"
        aria-label={t.importPick}
        disabled={busy}
        onChange={(e) => pick(e.target.files?.[0])}
      />
      {result ? (
        <>
          <div className="mono-block" style={{ marginTop: 12 }}>
            {JSON.stringify({ association: result.association, kind: result.receipt?.kind }, null, 2)}
          </div>
          <p className="small-note">
            {result.association === "linked" ? t.importLinked : t.importPending}
          </p>
          {result.snapshot_id ? (
            <div className="actions">
              <button className="primary" onClick={() => onDone(result.snapshot_id)}>
                {t.confirmCandidate}
              </button>
            </div>
          ) : (
            <div className="actions">
              <button className="secondary" onClick={() => onDone(null)}>{t.close}</button>
            </div>
          )}
        </>
      ) : null}
    </Modal>
  );
}

function ProfileModal({
  t, taskId, profile, onClose, onDone,
}: {
  t: T;
  taskId: string;
  profile: any | null;
  onClose: () => void;
  onDone: () => Promise<void>;
}) {
  const [exe, setExe] = useState(profile?.checks?.[0]?.exe ?? "");
  const [args, setArgs] = useState((profile?.checks?.[0]?.args ?? []).join(" "));
  const [err, setErr] = useState<ApiError | null>(null);
  const [busy, setBusy] = useState(false);
  const firstRef = useRef<HTMLInputElement>(null);

  return (
    <Modal title={t.profileTitle} onClose={onClose} initialFocusRef={firstRef}>
      <h2>{t.profileTitle}</h2>
      <p className="sub">{t.profileNote}</p>
      <ErrorNote error={err?.message} details={err?.details} />
      {profile ? (
        <div className="mono-block" style={{ marginBottom: 8 }}>
          {profile.checks.map((c: any) => `${c.exe} ${(c.args ?? []).join(" ")}`).join("\n")}
        </div>
      ) : null}
      {!profile ? (
        <>
          <label className="field">
            <span>{t.profileExe}</span>
            <input ref={firstRef as any} type="text" value={exe} onChange={(e) => setExe(e.target.value)} />
          </label>
          <label className="field">
            <span>{t.profileArgs}</span>
            <input type="text" value={args} onChange={(e) => setArgs(e.target.value)} />
          </label>
        </>
      ) : null}
      <div className="actions">
        <button
          className="primary"
          disabled={busy}
          onClick={async () => {
            setBusy(true);
            setErr(null);
            try {
              if (!profile) {
                await api.saveProfile(taskId, {
                  name: "local-checks",
                  checks: [
                    {
                      id: "check-1",
                      exe: exe.trim(),
                      args: args.split(" ").map((x: string) => x.trim()).filter(Boolean),
                      cwd_relative: "",
                      timeout_seconds: 600,
                      required: true,
                    },
                  ],
                  prep: [],
                  outputs: [],
                });
              }
              await onDone();
            } catch (e) {
              setErr(e as ApiError);
              setBusy(false);
            }
          }}
        >
          {t.approveRun}
        </button>
        <button className="secondary" onClick={onClose} disabled={busy}>{t.cancel}</button>
      </div>
    </Modal>
  );
}
