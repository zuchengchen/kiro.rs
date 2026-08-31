import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from "@/components/ui/dialog";
import { Badge } from "@/components/ui/badge";
import { useTraces } from "@/hooks/use-traces";
import type { TraceAttempt, TraceRecord } from "@/types/api";

interface CredentialFailuresDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  credentialId: number;
  email?: string;
  /**
   * `failed`（默认）：只看最终失败的请求，对应卡片「失败」栏。
   * `recovered`：只看最终成功的请求，对应卡片成功栏里的「已救回」。
   */
  mode?: "failed" | "recovered";
}

/** 失败分类 → 中文标签 + Badge 颜色 */
function outcomeStyle(outcome: string | null): {
  label: string;
  variant: "destructive" | "warning" | "outline" | "secondary";
} {
  switch (outcome) {
    case "quota_exhausted":
      return { label: "额度耗尽", variant: "warning" };
    case "account_throttled":
      return { label: "账号风控", variant: "warning" };
    case "auth_failed":
      return { label: "鉴权失败", variant: "destructive" };
    case "transient":
      return { label: "瞬态错误", variant: "outline" };
    case "network_error":
      return { label: "网络错误", variant: "destructive" };
    case "bad_request":
      return { label: "请求错误", variant: "destructive" };
    case "stream_interrupted":
      return { label: "流中断", variant: "warning" };
    default:
      return { label: outcome || "未知", variant: "secondary" };
  }
}

function formatTime(ts: string): string {
  const d = new Date(ts);
  if (isNaN(d.getTime())) return ts;
  return d.toLocaleString("zh-CN", { hour12: false });
}

function keySourceLabel(rec: TraceRecord): string {
  return rec.keyName ?? `#${rec.keyId}`;
}

export function CredentialFailuresDialog({
  open,
  onOpenChange,
  credentialId,
  email,
  mode = "failed",
}: CredentialFailuresDialogProps) {
  const recoveredOnly = mode === "recovered";
  const { data, isLoading } = useTraces(
    {
      failedAttemptCredentialId: credentialId,
      // 两个视图都在服务端按最终状态过滤。失败视图若不带 status，返回的 50 条里
      // 会混入大量已救回的请求（实测 50 条里 31 条最终成功），前端过滤后只剩十几条，
      // 看起来像“失败记录很少”。onlyFailed 让服务端只回最终失败的 trace。
      status: recoveredOnly ? "success" : undefined,
      onlyFailed: recoveredOnly ? undefined : true,
      limit: 50,
    },
    open,
  );
  const records = data?.records ?? [];
  // 摊平：同一请求里该凭据失败了几跳就显示几条（按时间倒序）
  const failedHops = records.flatMap((rec) =>
    rec.attempts
      .filter((a) => a.credentialId === credentialId && a.outcome !== "success")
      .map((a) => ({ rec, attempt: a })),
  );

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>
            {recoveredOnly ? "已救回的尝试" : "失败尝试详情"}
          </DialogTitle>
          <DialogDescription>
            {recoveredOnly ? (
              <>
                {email || `凭据 #${credentialId}`} 上失败过、但整条请求
                <span className="font-medium text-foreground">最终成功</span>
                的单次尝试（最多 50 条请求）。客户端未收到错误，这些请求已计入
                成功数，不计入失败数。
              </>
            ) : (
              <>
                {email || `凭据 #${credentialId}`} 上
                <span className="font-medium text-foreground">最终失败</span>
                请求里的单次尝试（最多 50 条请求，一个请求失败几跳就列几条）。
                重试或换桶救回的尝试不在此处，见成功栏的「已救回」。
              </>
            )}
          </DialogDescription>
        </DialogHeader>
        <div className="max-h-[60vh] space-y-2 overflow-y-auto">
          {isLoading ? (
            <div className="py-6 text-center text-sm text-muted-foreground">
              加载中…
            </div>
          ) : failedHops.length === 0 ? (
            <div className="py-6 text-center text-sm text-muted-foreground">
              {recoveredOnly
                ? "该凭据近期没有被救回的失败尝试。"
                : "该凭据暂无最终失败的尝试（trace 关闭或近期无失败）。"}
            </div>
          ) : (
            failedHops.map(({ rec, attempt }) => (
              <FailureRow
                key={`${rec.traceId}-${attempt.attempt}`}
                rec={rec}
                attempt={attempt}
              />
            ))
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}

/** 单跳失败：展示该凭据某次失败的 outcome / HTTP / 错误体 */
function FailureRow({
  rec,
  attempt,
}: {
  rec: TraceRecord;
  attempt: TraceAttempt;
}) {
  const style = outcomeStyle(attempt.outcome);
  // 这一跳失败了，但整条 trace 最终成功——客户端没有收到错误，该请求也已计入
  // 凭据的 successCount（瞬态 429 不进 totalFailureCount）。
  //
  // 救回方式分三种，区分开才能看清到底是什么在起作用：
  // - 同凭据同桶重试：上游瞬时限流，退避后同一入口就恢复了
  // - 同凭据换桶：429 后在另一个限流桶重发成功（桶配额独立）
  // - 转其他凭据：故障转移到别的账号后成功（需要账号池有可用目标）
  const traceRecovered = rec.finalStatus === "success";
  const sameCredential = attempt.credentialId === rec.finalCredentialId;
  // 最终成功那一跳的端点：用来判断是否真的换了桶。
  const successEndpoint = rec.attempts.find((a) => a.outcome === "success")
    ?.endpoint;
  const switchedBucket =
    successEndpoint != null && successEndpoint !== attempt.endpoint;

  let recoveryLabel: string;
  let recoveryHint: string;
  if (!sameCredential) {
    recoveryLabel = `本次请求最终成功（转由凭据 #${rec.finalCredentialId}）`;
    recoveryHint = `已故障转移到凭据 #${rec.finalCredentialId} 并成功，客户端未收到错误`;
  } else if (switchedBucket) {
    recoveryLabel = `本次请求最终成功（同凭据换桶 → ${successEndpoint}）`;
    recoveryHint = `该凭据改走 ${successEndpoint} 限流桶后成功，客户端未收到错误`;
  } else {
    recoveryLabel = "本次请求最终成功（同凭据同桶重试）";
    recoveryHint =
      "上游瞬时限流，退避后在同一入口重试成功，客户端未收到错误";
  }
  return (
    <div className="rounded-lg border border-border/50 bg-secondary/30 p-3">
      <div className="flex flex-wrap items-center gap-2 text-[13px]">
        <span className="tabular-nums text-muted-foreground">
          {formatTime(rec.ts)}
        </span>
        <Badge variant="secondary">{keySourceLabel(rec)}</Badge>
        <Badge variant={style.variant}>{style.label}</Badge>
        {attempt.httpStatus != null && (
          <span className="font-mono text-muted-foreground">
            HTTP {attempt.httpStatus}
          </span>
        )}
        {rec.totalAttempts > 1 && (
          <span className="text-[12px] text-muted-foreground">
            第 {attempt.attempt + 1}/{rec.totalAttempts} 跳
          </span>
        )}
        {traceRecovered && (
          <Badge variant="outline" title={recoveryHint}>
            {recoveryLabel}
          </Badge>
        )}
        {rec.finalStatus === "interrupted" && (
          <Badge variant="warning">中断</Badge>
        )}
        <span className="ml-auto text-[12px] text-muted-foreground">
          {rec.model}
        </span>
      </div>
      {attempt.errorSnippet && (
        <pre className="mt-2 max-h-32 overflow-auto whitespace-pre-wrap break-all rounded-md bg-background/60 p-2 font-mono text-[11px] text-muted-foreground">
          {attempt.errorSnippet}
        </pre>
      )}
    </div>
  );
}
