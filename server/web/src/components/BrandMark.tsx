export function BrandMark({ size = 34 }: { size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 36 36"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      aria-hidden="true"
    >
      <rect width="36" height="36" rx="9" fill="#3370FF" />
      <path
        d="M12.5 9.5C12.5 8.12 13.62 7 15 7C16.38 7 17.5 8.12 17.5 9.5V15H12.5V9.5Z"
        fill="white"
        fillOpacity="0.92"
      />
      <path
        d="M18.5 9.5C18.5 8.12 19.62 7 21 7C22.38 7 23.5 8.12 23.5 9.5V15H18.5V9.5Z"
        fill="white"
        fillOpacity="0.72"
      />
      <rect x="10" y="14.5" width="16" height="13" rx="6.5" fill="white" />
      <circle cx="15" cy="20.5" r="1.6" fill="#3370FF" />
      <circle cx="21" cy="20.5" r="1.6" fill="#3370FF" />
    </svg>
  );
}
