import { useEffect, useState } from 'react'
import { Alert, Button, Dialog, DialogContent, DialogHeader, DialogTitle, DialogBody, Spinner } from '@ui'
import type { ImageAsset } from './assistant-image-tools'
import type { Translate } from './assistant-read-types'

type Props = { images: ImageAsset[]; load: (id: string, signal?: AbortSignal) => Promise<Blob>; t: Translate }

function ImageResult({ image, load, t }: { image: ImageAsset; load: Props['load']; t: Translate }) {
  const [url, setUrl] = useState('')
  const [error, setError] = useState('')
  const [open, setOpen] = useState(false)
  useEffect(() => {
    const controller = new AbortController()
    let source = ''
    setError('')
    setUrl('')
    void load(image.id, controller.signal).then(blob => {
      if (controller.signal.aborted) return
      source = URL.createObjectURL(blob)
      setUrl(source)
    }).catch(failure => {
      if (!controller.signal.aborted) setError(failure instanceof Error ? failure.message : String(failure))
    })
    return () => { controller.abort(); if (source) URL.revokeObjectURL(source) }
  }, [image.id, load])
  const extension = image.mime === 'image/jpeg' ? 'jpg' : image.mime === 'image/webp' ? 'webp' : 'png'
  return (
    <figure style={{ margin: 0, minWidth: 0 }}>
      {error ? <Alert variant='destructive'>{error}</Alert> : url ? (
        <Button variant='ghost' onClick={() => setOpen(true)} aria-label={t('查看图片')}
          style={{ padding: 0, width: '100%', height: 'auto', overflow: 'hidden' }}>
          <img src={url} alt={t('生成的图片')} style={{ display: 'block', width: '100%', maxHeight: 320, objectFit: 'contain' }} />
        </Button>
      ) : <Spinner aria-label={t('正在读取图片…')} />}
      <figcaption style={{ display: 'flex', gap: 8, alignItems: 'center', fontSize: 12, marginTop: 6 }}>
        <span>{image.width} × {image.height}</span>
        {url ? <a href={url} download={`image-${image.id.slice(0, 8)}.${extension}`}>{t('下载图片')}</a> : null}
      </figcaption>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent>
          <DialogHeader><DialogTitle>{t('生成的图片')}</DialogTitle></DialogHeader>
          <DialogBody><img src={url} alt={t('生成的图片')} style={{ maxWidth: '100%', maxHeight: '75vh', objectFit: 'contain' }} /></DialogBody>
        </DialogContent>
      </Dialog>
    </figure>
  )
}

export function AssistantImageResults({ images, load, t }: Props) {
  return <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(min(220px, 100%), 1fr))', gap: 12, padding: 12 }}>
    {images.map(image => <ImageResult key={image.id} image={image} load={load} t={t} />)}
  </div>
}
