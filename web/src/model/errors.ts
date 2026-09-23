export class ModelError extends Error {
  constructor(
    readonly key: string,
    readonly path?: string,
    readonly params: Readonly<Record<string, string | number>> = {},
  ) {
    super(key);
    this.name = "ModelError";
  }
}
