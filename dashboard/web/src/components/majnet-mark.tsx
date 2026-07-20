import { useId } from 'react'

/** The MajNet logo mark — an M/N monogram built from network edges with a teal
 * accent node. The ink strokes use `currentColor` so it adapts to light/dark;
 * the accent node stays brand teal. */
export function MajnetMark({ className }: { className?: string }) {
  const mask = useId()
  return (
    <svg
      viewBox="0 0 64 64"
      className={className}
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      aria-hidden="true"
    >
      <defs>
        <mask id={mask}>
          <rect x="-4" y="-4" width="72" height="72" fill="white" />
          <circle cx="11" cy="17" r="8.5" fill="black" />
          <circle cx="53" cy="17" r="8.5" fill="black" />
          <circle cx="32" cy="41" r="7.9" fill="black" />
        </mask>
      </defs>
      <g
        mask={`url(#${mask})`}
        fill="none"
        stroke="currentColor"
        strokeWidth="4"
        strokeLinecap="round"
        strokeLinejoin="round"
      >
        <path d="M 11 49 L 11 17 L 32 41 L 53 17 L 53 49" />
        <path d="M 11 17 A 29 29 0 0 1 53 17" />
      </g>
      <circle cx="32" cy="41" r="5.7" fill="#0FBFB2" />
      <circle cx="11" cy="17" r="4.3" fill="none" stroke="currentColor" strokeWidth="4" />
      <circle cx="53" cy="17" r="4.3" fill="none" stroke="currentColor" strokeWidth="4" />
    </svg>
  )
}
