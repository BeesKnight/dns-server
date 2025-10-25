import { useState } from "react";
import MapGlobe from "../components/MapGlobe";
import CheckSheet from "../components/CheckSheet";

export default function Home() {
  const [open, setOpen] = useState(true);

  return (
    <div className="relative min-h-screen overflow-hidden bg-[#0b1220]">
      {/* мягкая подсветка центра */}
      <div className="pointer-events-none absolute inset-0 bg-[radial-gradient(circle_at_center,rgba(37,99,235,0.25),transparent_55%)]" />

      {/* центрируем глобус */}
      <div className="px-4 py-8 md:py-10">
        <MapGlobe />
      </div>

      {/* нижний выезжающий шит */}
      <CheckSheet open={open} onOpenChange={setOpen} />
    </div>
  );
}
