import { Link } from "react-router-dom";
import type {
  ButtonHTMLAttributes,
  AnchorHTMLAttributes,
  ReactNode,
} from "react";
import type { LinkProps } from "react-router-dom";

type BaseProps = {
  children: ReactNode;
  iconLeft?: ReactNode;
  iconRight?: ReactNode;
  size?: "sm" | "md";
  className?: string;
};

type LinkVariantProps = BaseProps &
  Omit<LinkProps, "className" | "to" | "type"> & {
    to: LinkProps["to"];
    href?: undefined;
  };

type AnchorVariantProps = BaseProps &
  Omit<AnchorHTMLAttributes<HTMLAnchorElement>, "className" | "href" | "type"> & {
    href: string;
    to?: undefined;
  };

type ButtonVariantProps = BaseProps &
  Omit<ButtonHTMLAttributes<HTMLButtonElement>, "className"> & {
    to?: undefined;
    href?: undefined;
  };

type Props = LinkVariantProps | AnchorVariantProps | ButtonVariantProps;

const base =
  "inline-flex items-center gap-2 rounded-2xl border border-white/10 " +
  "bg-slate-900/55 hover:bg-slate-900/70 backdrop-blur-xl " +
  "text-slate-200 transition shadow-lg";

const sizes = {
  sm: "px-3 py-1.5 text-sm",
  md: "px-4 py-2 text-[15px]",
};

export default function GlassButton(props: Props) {
  if ("to" in props && props.to) {
    const {
      to,
      children,
      iconLeft,
      iconRight,
      size = "md",
      className = "",
      ...rest
    } = props as LinkVariantProps;
    const cls = `${base} ${sizes[size]} ${className}`.trim();
    return (
      <Link to={to} className={cls} {...rest}>
        {iconLeft}
        {children}
        {iconRight}
      </Link>
    );
  }

  if ("href" in props && props.href) {
    const {
      href,
      children,
      iconLeft,
      iconRight,
      size = "md",
      className = "",
      ...rest
    } = props as AnchorVariantProps;
    const cls = `${base} ${sizes[size]} ${className}`.trim();
    return (
      <a href={href} className={cls} {...rest}>
        {iconLeft}
        {children}
        {iconRight}
      </a>
    );
  }

  const { children, iconLeft, iconRight, size = "md", className = "", ...rest } =
    props as ButtonVariantProps;
  const cls = `${base} ${sizes[size]} ${className}`.trim();
  return (
    <button className={cls} {...rest}>
      {iconLeft}
      {children}
      {iconRight}
    </button>
  );
}
