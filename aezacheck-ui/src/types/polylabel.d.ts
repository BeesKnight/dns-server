declare module 'polylabel' {
  type Point = [number, number];
  export default function polylabel(
    polygon: Point[][] | Point[][][],
    precision?: number,
    debug?: boolean
  ): Point;
}
