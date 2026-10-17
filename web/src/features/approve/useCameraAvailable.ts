import { useEffect, useState } from 'react'
import { cameraAvailable } from './scanner'

/** 这台设备能不能扫码（有摄像头、安全上下文）。异步查一次；查到之前当作没有。 */
export function useCameraAvailable(): boolean {
  const [ok, setOk] = useState(false)
  useEffect(() => {
    let alive = true
    void cameraAvailable().then((v) => alive && setOk(v))
    return () => {
      alive = false
    }
  }, [])
  return ok
}
