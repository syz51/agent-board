/** `factory-12` → `factory`. */
export function boardOfTask(taskId: string): string {
  return taskId.replace(/-\d+$/, "");
}
