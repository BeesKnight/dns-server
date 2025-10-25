import { Link } from "react-router-dom";
import type { ButtonHTMLAttributes, AnchorHTMLAttributes, ReactNode } from "react";

type Props = {
  children: ReactNode;
  iconLeft?: ReactNode;
  iconRight?: ReactNode;
  to?: string;        // для <Link>
  href?: string;      // для внешних ссылок
  size?: "sm" | "md";
  className?: string;
} & Omit<ButtonHTMLAttributes<HTMLButtonElement>, "className"> &
  Omit<AnchorHTMLAttributes<HTMLAnchorElement>, "className">;

const base =
  "inline-flex items-center gap-2 rounded-2xl border border-white/10 " +
  "bg-slate-900/55 hover:bg-slate-900/70 backdrop-blur-xl " +
  "text-slate-200 transition shadow-lg";

const sizes = {
  sm: "px-3 py-1.5 text-sm",
  md: "px-4 py-2 text-[15px]",
};

export default function GlassButton({
  children,
  iconLeft,
  iconRight,
  to,
  href,
  size = "md",
  className = "",
  ...rest
}: Props) {
  const cls = `${base} ${sizes[size]} ${className}`.trim();

  if (to) {
    return (
      <Link to={to} className={cls} {...(rest as any)}>
        {iconLeft}{children}{iconRight}
      </Link>
    );
  }
  if (href) {
    return (
      <a href={href} className={cls} {...(rest as any)}>
        {iconLeft}{children}{iconRight}
      </a>
    );
  }
  return (
    <button className={cls} {...(rest as any)}>
      {iconLeft}{children}{iconRight}
    </button>
  );
}