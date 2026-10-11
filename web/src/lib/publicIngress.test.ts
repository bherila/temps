// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import { publicIngressBaseUrl, publicIngressCallbackUrl } from './publicIngress'

const CONSOLE_ORIGIN = 'https://console.example.com'

describe('publicIngressBaseUrl', () => {
  test('uses the URL the API reports, not the address being browsed', () => {
    expect(
      publicIngressBaseUrl(
        { effective_url: 'https://hooks.example.com' },
        CONSOLE_ORIGIN
      )
    ).toBe('https://hooks.example.com')
  })

  test('strips trailing slashes and surrounding whitespace', () => {
    expect(
      publicIngressBaseUrl(
        { effective_url: '  https://hooks.example.com//  ' },
        CONSOLE_ORIGIN
      )
    ).toBe('https://hooks.example.com')
  })

  test('falls back to the browser origin while the status is unavailable', () => {
    expect(publicIngressBaseUrl(undefined, CONSOLE_ORIGIN)).toBe(CONSOLE_ORIGIN)
    expect(publicIngressBaseUrl(null, `${CONSOLE_ORIGIN}/`)).toBe(
      CONSOLE_ORIGIN
    )
    expect(publicIngressBaseUrl({ effective_url: '   ' }, CONSOLE_ORIGIN)).toBe(
      CONSOLE_ORIGIN
    )
    expect(publicIngressBaseUrl({ effective_url: null }, CONSOLE_ORIGIN)).toBe(
      CONSOLE_ORIGIN
    )
  })
})

describe('publicIngressCallbackUrl', () => {
  test('joins a base and an absolute path with exactly one slash', () => {
    expect(
      publicIngressCallbackUrl(
        'https://hooks.example.com',
        '/api/webhook/git/github/events'
      )
    ).toBe('https://hooks.example.com/api/webhook/git/github/events')
    expect(
      publicIngressCallbackUrl(
        'https://hooks.example.com/',
        'api/agents/webhook/wh_1'
      )
    ).toBe('https://hooks.example.com/api/agents/webhook/wh_1')
  })
})
