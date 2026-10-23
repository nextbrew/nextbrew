// The mark and wordmark; CSS shows the variant matching the theme.
export function Logo() {
  return (
    <span className="nb-logo">
      <img src="/brand/mark.svg" alt="" width="24" height="24" className="nb-light" />
      <img src="/brand/mark-dark.svg" alt="" width="24" height="24" className="nb-dark" />
      <img src="/brand/wordmark.svg" alt="nextbrew" height="18" className="nb-light" />
      <img src="/brand/wordmark-dark.svg" alt="nextbrew" height="18" className="nb-dark" />
    </span>
  )
}
