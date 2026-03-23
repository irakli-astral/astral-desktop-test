import { render } from '@/test/test-utils'
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { check } from '@tauri-apps/plugin-updater'
import App from './App'

// Tauri bindings are mocked globally in src/test/setup.ts

describe('App', () => {
  it('renders without crashing', () => {
    const { container } = render(<App />)
    expect(container).toBeTruthy()
  })
})

describe('auto-update', () => {
  beforeEach(() => {
    vi.useFakeTimers()
  })
  afterEach(() => {
    vi.useRealTimers()
  })

  it('checks for updates 5s after mount', async () => {
    render(<App />)
    await vi.advanceTimersByTimeAsync(5000)
    expect(check).toHaveBeenCalledOnce()
  })
})
