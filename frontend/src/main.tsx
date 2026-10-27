import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter, Route, Routes } from 'react-router-dom'
import './index.css'
import { Home } from './Home'
import { PaperPage } from './PaperPage'
import { Shell } from './Shell'

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <BrowserRouter basename="/paper-bro">
      <Shell>
        <Routes>
          <Route path="/" element={<Home />} />
          <Route path="/abs/*" element={<PaperPage />} />
          <Route path="/pdf/*" element={<PaperPage />} />
          <Route path="/html/*" element={<PaperPage />} />
          <Route path="*" element={<Home />} />
        </Routes>
      </Shell>
    </BrowserRouter>
  </StrictMode>,
)
