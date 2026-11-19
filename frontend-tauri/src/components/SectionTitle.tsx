import { clsx } from 'clsx'

interface SectionTitleProps {
  children: React.ReactNode
  action?: React.ReactNode
  className?: string
}

export function SectionTitle({ children, action, className }: SectionTitleProps) {
  return (
    <div className={clsx('flex items-center justify-between mb-4', className)}>
      <h2 className="text-lg font-semibold text-orcave-text">{children}</h2>
      {action && <div>{action}</div>}
    </div>
  )
}