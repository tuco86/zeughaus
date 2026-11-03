// The CI's `/ci` API over weida, the way zeughaus-runner/src/mcp/client.rs asks it: the pinned URL
// names the CI's fingerprint, and this machine's client key (the editor's `client.pem`) is what
// the CI admits. The only module that imports @tuco86/weida, and only at the first question, so
// loading the extension never fails where the addon has no prebuild.
//
// One runtime and one connected requester are made on first use and kept; a requester whose
// exchange failed is dropped and dialled again by the next question. Every wait is bounded, so a
// CI that does not answer is an error and not a call that never returns.

import { readFileSync } from "node:fs";
import { join } from "node:path";
import type * as Weida from "@tuco86/weida";
import {
  type CiReply,
  type CiRequest,
  MAX_CI_REPLY_BYTES,
  ciUrl,
  decodeReply,
  encodeRequest,
  pickCiUrl,
  retries,
  stateDir,
} from "./protocol.ts";

/** How long a first dial may take. */
const CONNECT_MS = 20_000;
/** How long one question may take. */
const CI_MS = 30_000;
const SHUTDOWN_MS = 1500;

const message = (e: unknown): string => (e instanceof Error ? e.message : String(e));

/** A config file's text, `null` when it does not exist. */
function readConfig(path: string): string | null {
  try {
    return readFileSync(path, "utf8");
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === "ENOENT") return null;
    throw new Error(`cannot read ${path}: ${message(e)}`);
  }
}

/** The CI to dial and how: resolved at the first question, kept once it resolved. */
interface Link {
  weida: typeof Weida;
  url: string;
  tls: Weida.ClientTls;
}

export class CiClient {
  #link: Link | undefined;
  #runtime: Weida.Runtime | undefined;
  #requester: Weida.Requester | undefined;
  #connecting: Promise<Weida.Requester> | undefined;

  /**
   * Asks the CI one question; its refusal is the error. A question that only reads is asked once
   * more on a fresh connection when the exchange broke off, which is what a CI restart looks like
   * from here; one that changes state is not, because the CI may have taken it.
   */
  async ask(request: CiRequest, signal?: AbortSignal): Promise<CiReply> {
    const first = await this.#once(request, signal);
    if (!(first instanceof Error)) return first;
    if (!retries(request)) throw first;
    const second = await this.#once(request, signal);
    if (second instanceof Error) throw second;
    return second;
  }

  /** Closes the connection, so the CI sees this client leave; a later question dials anew. */
  async close(): Promise<void> {
    this.#requester?.drop();
    this.#requester = undefined;
    const runtime = this.#runtime;
    this.#runtime = undefined;
    await runtime?.shutdown({ signal: AbortSignal.timeout(SHUTDOWN_MS) }).catch(() => undefined);
  }

  /** One exchange. A broken one is returned (it may be retried); every other failure throws. */
  async #once(request: CiRequest, signal?: AbortSignal): Promise<CiReply | Error> {
    const body = encodeRequest(request);
    const requester = await this.#connected(signal);
    const limit = AbortSignal.timeout(CI_MS);
    const both = signal ? AbortSignal.any([signal, limit]) : limit;
    let payload: Uint8Array;
    try {
      const transfer = await requester.request(body, { signal: both });
      ({ payload } = await transfer.collect(MAX_CI_REPLY_BYTES, { signal: both }));
    } catch (e) {
      if (this.#requester === requester) {
        this.#requester = undefined;
        requester.drop();
      }
      if (limit.aborted) throw new Error(`ci: no answer within ${CI_MS / 1000} s`);
      if (signal?.aborted) throw new Error("ci: no answer in time");
      return new Error(`ci: ${message(e)}`);
    }
    return decodeReply(payload);
  }

  /** The connected requester, dialled now if there is none; concurrent callers share one dial. */
  async #connected(signal?: AbortSignal): Promise<Weida.Requester> {
    if (this.#requester) return this.#requester;
    this.#connecting ??= this.#connect().finally(() => {
      this.#connecting = undefined;
    });
    if (!signal) return this.#connecting;
    // The dial runs under its own limit; the caller only stops waiting for it.
    const aborted = Promise.withResolvers<never>();
    const stop = (): void => aborted.reject(new Error("ci: no answer in time"));
    if (signal.aborted) stop();
    else signal.addEventListener("abort", stop, { once: true });
    try {
      return await Promise.race([this.#connecting, aborted.promise]);
    } finally {
      signal.removeEventListener("abort", stop);
    }
  }

  async #connect(): Promise<Weida.Requester> {
    const link = await this.#resolve();
    this.#runtime ??= link.weida.Runtime.owned();
    const runtime = this.#runtime;
    const requester = runtime.requester(link.tls);
    const limit = AbortSignal.timeout(CONNECT_MS);
    try {
      await requester.connect(link.url, { signal: limit });
    } catch (e) {
      requester.drop();
      const why = limit.aborted ? `no answer within ${CONNECT_MS / 1000} s` : message(e);
      throw new Error(`connect ${link.url}: ${why}`);
    }
    if (this.#runtime !== runtime) {
      requester.drop();
      throw new Error("ci: the client was closed during the dial");
    }
    this.#requester = requester;
    return requester;
  }

  /**
   * The CI is `$ZEUGHAUS_CI`, else the first of `ci = [..]` in the state directory's
   * `zeughaus.toml`, else the `ci` of its `enlist.toml`; the identity is its `client.pem`.
   */
  async #resolve(): Promise<Link> {
    if (this.#link) return this.#link;
    // On a platform without a prebuild this rejects with weida's own reason.
    const weida = await import("@tuco86/weida");
    const dir = stateDir(process.env, process.platform);
    const editor = join(dir, "zeughaus.toml");
    const enlist = join(dir, "enlist.toml");
    const url = ciUrl(
      pickCiUrl(
        process.env.ZEUGHAUS_CI,
        { path: editor, text: readConfig(editor) },
        { path: enlist, text: readConfig(enlist) },
      ),
    );
    const pem = join(dir, "client.pem");
    let identity: Weida.Identity;
    try {
      identity = weida.Identity.fromPemFile(pem);
      identity.fingerprint();
    } catch {
      throw new Error(`no client identity in ${pem}`);
    }
    this.#link = { weida, url, tls: new weida.ClientTls(new weida.Trust(), { identity }) };
    return this.#link;
  }
}
