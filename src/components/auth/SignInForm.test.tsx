import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { render, screen, waitFor } from '@/test/test-utils'

const mockCreate = vi.fn()
const mockSetActive = vi.fn()
const mockPopupOnce = vi.fn()
const mockGetByLabel = vi.fn()

vi.mock('@clerk/clerk-react', () => ({
  useSignIn: vi.fn(() => ({
    isLoaded: true,
    signIn: { create: mockCreate },
    setActive: mockSetActive,
  })),
}))

const mockInvoke = vi.fn()

vi.mock('@tauri-apps/api/core', () => ({
  invoke: mockInvoke,
}))

vi.mock('@tauri-apps/api/webviewWindow', () => ({
  WebviewWindow: {
    getByLabel: mockGetByLabel,
  },
}))

const { listen } = await import('@tauri-apps/api/event')
const { SignInForm } = await import('./SignInForm')

describe('SignInForm', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    vi.mocked(listen).mockResolvedValue(vi.fn())
    mockInvoke.mockResolvedValue(undefined)
    mockGetByLabel.mockResolvedValue({
      once: mockPopupOnce,
    })
    mockCreate.mockResolvedValue({
      firstFactorVerification: {
        externalVerificationRedirectURL: new URL(
          'https://accounts.google.com/o/oauth2/v2/auth'
        ),
      },
    })
  })

  it('forces a fresh Google login in the OAuth popup flow', async () => {
    const user = userEvent.setup()

    render(<SignInForm />)

    await user.click(
      screen.getByRole('button', { name: 'Sign in with Google' })
    )

    await waitFor(() => {
      expect(mockCreate).toHaveBeenCalledWith({
        strategy: 'oauth_google',
        redirectUrl: window.location.origin + '/sso-callback',
        actionCompleteRedirectUrl: window.location.origin,
        oidcPrompt: 'select_account',
      })
    })

    expect(mockInvoke).toHaveBeenCalledWith('open_oauth_popup', {
      url: 'https://accounts.google.com/o/oauth2/v2/auth',
    })
    expect(mockGetByLabel).toHaveBeenCalledWith('oauth-popup')
    expect(listen).toHaveBeenCalledWith(
      'clerk-auth-complete',
      expect.any(Function)
    )
    expect(listen).toHaveBeenCalledWith(
      'oauth-signup-blocked',
      expect.any(Function)
    )
    expect(mockPopupOnce).toHaveBeenCalledTimes(2)
  })
})
