import { ModelError } from "./errors";

export interface ListenerParts {
  address: string;
  port: string;
}

/** Decompose a server listen value without normalizing its displayed parts. */
export function splitListener(value: string | null | undefined): ListenerParts {
  if (!value) return { address: "", port: "" };
  const separator = value.lastIndexOf(":");
  if (separator < 0) return { address: value, port: "" };
  const address = value.slice(0, separator);
  return {
    address: address.startsWith("[") && address.endsWith("]") ? address.slice(1, -1) : address,
    port: value.slice(separator + 1),
  };
}

/** Serialize only at preview/apply time; the caller retains raw draft parts. */
export function joinListener(addressDraft: string, portDraft: string): string {
  let address = addressDraft.trim();
  const port = portDraft.trim();
  if (!address) throw new ModelError("settings.listener.address.required", "address");
  if (address.startsWith("[") && address.endsWith("]") && address.includes(":")) address = address.slice(1, -1);
  if (!address || /[\s\[\]\/]/.test(address) || (address.includes("%") && !address.includes(":"))) {
    throw new ModelError("settings.listener.address.invalid", "address");
  }
  if (!/^[0-9]+$/.test(port) || Number(port) > 65535) {
    throw new ModelError("settings.listener.port.invalid", "port");
  }
  return `${address.includes(":") ? `[${address}]` : address}:${Number(port)}`;
}
