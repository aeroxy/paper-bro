/** The academies' design tokens, so paper-bro and its courses look like one family. */
export default {
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  theme: {
    extend: {
      colors: {
        ink: { DEFAULT: '#0B0B0B', 700: '#26262A', 500: '#52525B', 300: '#A1A1AA' },
        action: { DEFAULT: '#D85A30', 700: '#C24E27', 300: '#ECA88F', soft: '#FDF3EF' },
        status: { DEFAULT: '#2E6BE6', 700: '#1D4CA6', 300: '#9DBAF7', soft: '#EFF4FE' },
        paper: '#FCFCF9',
        sunken: '#FAFAF8',
        muted: '#F4F4F5',
        line: { DEFAULT: '#E4E4E7', strong: '#D4D4D8' },
        fg: { DEFAULT: '#0B0B0B', 2: '#52525B', 3: '#71717A' },
        danger: { DEFAULT: '#DC2626', strong: '#B91C1C', soft: '#FEF2F2' },
      },
      fontFamily: {
        sans: ['Inter', '-apple-system', 'BlinkMacSystemFont', 'Segoe UI', 'Helvetica Neue', 'Arial', 'sans-serif'],
        mono: ['"JetBrains Mono"', 'ui-monospace', 'SF Mono', 'Menlo', 'monospace'],
      },
      boxShadow: {
        card: '0 1px 0 0 rgba(11,11,11,.04),0 12px 24px -12px rgba(11,11,11,.10)',
      },
    },
  },
}
