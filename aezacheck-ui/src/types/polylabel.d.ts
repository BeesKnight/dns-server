declare module '@mapbox/polylabel' {
  type Ring = number[][];
  type Polygon = Ring[];
  export default function polylabel(
    polygon: Polygon | Polygon[],
    precision?: number,
    debug?: boolean
  ): [number, number];
}
