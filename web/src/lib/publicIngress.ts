// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Public ingress (ADR-051): the base URL Temps hands to third parties that
 * call back into it. It is not necessarily the address the operator is
 * browsing — that one is the console and may be restricted to an allowlist —
 * so a callback URL shown in the console must be built from the API's answer,
 * never from `window.location.origin`.
 */

/** The part of `GET /settings/public-ingress` URL building needs. */
export interface PublicIngressUrlSource {
  effective_url?: string | null
}

/**
 * Base URL for anything a third party will request, with no trailing slash.
 *
 * `fallbackOrigin` covers the cases where the status is unavailable: it has
 * not loaded yet, or the viewer may not read settings. The browser's origin is
 * the best remaining guess, and is what the console used before.
 */
export function publicIngressBaseUrl(
  status: PublicIngressUrlSource | null | undefined,
  fallbackOrigin: string
): string {
  const configured = status?.effective_url?.trim().replace(/\/+$/, '')
  return configured || fallbackOrigin.replace(/\/+$/, '')
}

/** Join an ingress base URL and an absolute path such as `/api/webhook/...`. */
export function publicIngressCallbackUrl(
  baseUrl: string,
  path: string
): string {
  const base = baseUrl.replace(/\/+$/, '')
  return `${base}${path.startsWith('/') ? path : `/${path}`}`
}
