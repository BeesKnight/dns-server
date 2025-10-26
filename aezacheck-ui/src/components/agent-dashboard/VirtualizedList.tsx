import type { ReactNode } from "react";
import { useEffect, useMemo, useRef, useState } from "react";

export interface VirtualizedListProps<T> {
  items: readonly T[];
  itemHeight: number;
  overscan?: number;
  className?: string;
  innerClassName?: string;
  keyExtractor?: (item: T, index: number) => string | number;
  renderItem: (item: T, index: number) => ReactNode;
  emptyState?: ReactNode;
}

type SizeState = {
  height: number;
  scrollTop: number;
};

const defaultExtractor = <T,>(item: T, index: number) => index;

export function VirtualizedList<T>({
  items,
  itemHeight,
  overscan = 4,
  className,
  innerClassName,
  keyExtractor = defaultExtractor,
  renderItem,
  emptyState,
}: VirtualizedListProps<T>) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const [{ height, scrollTop }, setSize] = useState<SizeState>({ height: 0, scrollTop: 0 });

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const handleScroll = () => {
      setSize((prev) => ({ ...prev, scrollTop: container.scrollTop }));
    };

    const resizeObserver = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (entry) {
        const newHeight = entry.contentRect.height;
        setSize((prev) => ({ ...prev, height: newHeight }));
      }
    });

    resizeObserver.observe(container);
    container.addEventListener("scroll", handleScroll);
    setSize((prev) => ({ ...prev, height: container.clientHeight }));

    return () => {
      resizeObserver.disconnect();
      container.removeEventListener("scroll", handleScroll);
    };
  }, []);

  const { startIndex, endIndex } = useMemo(() => {
    if (items.length === 0) return { startIndex: 0, endIndex: 0 };
    const visibleCount = Math.ceil(height / itemHeight);
    const start = Math.max(Math.floor(scrollTop / itemHeight) - overscan, 0);
    const end = Math.min(items.length, start + visibleCount + overscan * 2);
    return { startIndex: start, endIndex: end };
  }, [height, itemHeight, items.length, overscan, scrollTop]);

  if (!items.length && emptyState) {
    return <div className={className}>{emptyState}</div>;
  }

  const offsetY = startIndex * itemHeight;
  const totalHeight = items.length * itemHeight;
  const visibleItems = items.slice(startIndex, endIndex);

  return (
    <div ref={containerRef} className={className}>
      <div
        className={innerClassName}
        style={{
          position: "relative",
          height: totalHeight,
        }}
      >
        <div style={{ position: "absolute", top: offsetY, left: 0, right: 0 }}>
          {visibleItems.map((item, index) => (
            <div key={keyExtractor(item, startIndex + index)} style={{ height: itemHeight }}>
              {renderItem(item, startIndex + index)}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
