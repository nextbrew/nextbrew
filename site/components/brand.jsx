// The mark and wordmark; CSS shows the variant matching the theme.
export function Logo() {
  return (
    <span className="nb-logo">
      <img src="/brand/mark.svg" alt="" width="28" height="28" className="nb-mark nb-light" />
      <img src="/brand/mark-dark.svg" alt="" width="28" height="28" className="nb-mark nb-dark" />
      <img src="/brand/wordmark.svg" alt="nextbrew" height="22" className="nb-word nb-light" />
      <img src="/brand/wordmark-dark.svg" alt="nextbrew" height="22" className="nb-word nb-dark" />
    </span>
  )
}
