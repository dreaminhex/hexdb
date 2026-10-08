import { useMemo } from "react"
import qrcode from "qrcode-generator"

/** A QR code drawn as SVG (black on white, so phone cameras read it in either theme). */
export function QrCode({ value, size = 192, label }: { value: string; size?: number; label: string }) {
  const { count, path } = useMemo(() => {
    const qr = qrcode(0, "M")
    qr.addData(value)
    qr.make()
    const count = qr.getModuleCount()
    let path = ""
    for (let row = 0; row < count; row++) {
      for (let col = 0; col < count; col++) {
        if (qr.isDark(row, col)) path += `M${col} ${row}h1v1h-1z`
      }
    }
    return { count, path }
  }, [value])
  const margin = 4
  const box = count + margin * 2
  return (
    <svg role="img" aria-label={label} width={size} height={size} viewBox={`${-margin} ${-margin} ${box} ${box}`} shapeRendering="crispEdges" className="rounded-md">
      <rect x={-margin} y={-margin} width={box} height={box} fill="#fff" />
      <path d={path} fill="#000" />
    </svg>
  )
}
