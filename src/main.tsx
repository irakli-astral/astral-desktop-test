import ReactDOM from 'react-dom/client'
import { ClerkProvider } from '@clerk/clerk-react'
import { getCurrentWindow } from '@tauri-apps/api/window'
import App from './App'
import { OAuthPopupGuard } from './components/auth/SSOCallback'
import './App.css'

const PUBLISHABLE_KEY = import.meta.env.VITE_CLERK_PUBLISHABLE_KEY as string
const isOAuthPopup = getCurrentWindow().label === 'oauth-popup'

ReactDOM.createRoot(document.getElementById('root') as HTMLElement).render(
  <ClerkProvider publishableKey={PUBLISHABLE_KEY}>
    {isOAuthPopup ? <OAuthPopupGuard /> : <App />}
  </ClerkProvider>
)
