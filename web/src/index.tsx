import ReactDOM from 'react-dom/client'
import { TempsConsole } from './App'

// ui.sh picker toolbar (temporary — for comparing UI variants). Idempotent.
if (!document.querySelector('script[data-uidotsh-picker]')) {
  const s = document.createElement('script')
  s.src = 'https://ui.sh/ui-picker.js'
  s.setAttribute('data-uidotsh-picker', '')
  document.body.appendChild(s)
}

const rootEl = document.getElementById('root')
if (rootEl) {
  const root = ReactDOM.createRoot(rootEl)
  root.render(<TempsConsole />)
}
