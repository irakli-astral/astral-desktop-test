import { usePlatform } from '@/hooks/use-platform'
import { WindowsWindowControls } from './WindowsWindowControls'

/**
 * Title bar for the tunnel app.
 *
 * macOS: decorations=true + titleBarStyle="Transparent" — native traffic lights
 * are rendered by the OS. This div is a drag region and spacer only.
 *
 * Windows: decorations=false — custom window controls on the right.
 *
 * Linux: decorations=true — native WM handles everything, render nothing.
 */
export function TunnelTitleBar() {
  const platform = usePlatform()

  // Linux: native decorations provide window controls
  if (platform === 'linux') return null

  if (platform === 'windows') {
    return (
      <div
        data-tauri-drag-region
        style={{
          display: 'flex',
          width: '100%',
          flexShrink: 0,
          alignItems: 'center',
          justifyContent: 'flex-end',
        }}
      >
        <WindowsWindowControls />
      </div>
    )
  }

  // macOS: titleBarStyle="Transparent" — native traffic lights overlay this div.
  // Height matches the standard macOS title bar so content starts below controls.
  return (
    <div
      data-tauri-drag-region
      style={{
        width: '100%',
        flexShrink: 0,
      }}
    />
  )
}
