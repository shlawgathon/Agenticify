import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

type TaskRunStatus = {
  active: boolean;
  run_id: string | null;
  owner: string | null;
  started_unix_ms: number | null;
};

const ACTIVE_TASK_ERROR = "Task run already active";

export function isTaskRunConflictError(err: unknown): boolean {
  return String(err).includes(ACTIVE_TASK_ERROR);
}

export async function withTaskRunLock<T>(
  operation: string,
  fn: () => Promise<T>,
): Promise<T> {
  const owner = `${getCurrentWindow().label}:${operation}`;

  const lease = await invoke<TaskRunStatus>("begin_task_run_cmd", {
    owner,
  });

  try {
    return await fn();
  } finally {
    if (lease.run_id) {
      await invoke<TaskRunStatus>("end_task_run_cmd", {
        run_id: lease.run_id,
      }).catch(() => undefined);
    }
  }
}
