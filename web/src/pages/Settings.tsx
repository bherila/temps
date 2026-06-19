import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { Switch } from '@/components/ui/switch'
import { useBreadcrumbs } from '@/contexts/BreadcrumbContext'
import { usePageTitle } from '@/hooks/usePageTitle'
import {
  useSettings,
  useUpdateSettings,
  type PlatformSettings,
} from '@/hooks/useSettings'
import { client } from '@/api/client/client.gen'
import {
  AlertCircle,
  Globe,
  Image,
  Link,
  Loader2,
  RefreshCw,
  Save,
} from 'lucide-react'
import { useEffect, useState } from 'react'
import { useForm, useWatch } from 'react-hook-form'
import { toast } from 'sonner'

type SettingsFormData = Pick<
  PlatformSettings,
  | 'external_url'
  | 'internal_url'
  | 'preview_domain'
  | 'public_hostnames'
  | 'screenshots'
>

function optionalTemplate(value: string | null | undefined): string | null {
  const trimmed = value?.trim() ?? ''
  return trimmed.length > 0 ? trimmed : null
}

export function Settings() {
  const { setBreadcrumbs } = useBreadcrumbs()
  const { data: settings, isLoading, error } = useSettings()
  const updateSettings = useUpdateSettings()
  const [isRefreshingRoutes, setIsRefreshingRoutes] = useState(false)

  const {
    register,
    handleSubmit,
    control,
    formState: { isDirty, isSubmitting, errors },
    reset,
    setValue,
  } = useForm<SettingsFormData>({
    defaultValues: {
      external_url: '',
      internal_url: '',
      preview_domain: 'localho.st',
      public_hostnames: {
        strategy: 'standard',
        environment_template: '',
        service_template: '',
        deployment_template: '',
      },
      screenshots: {
        enabled: false,
        provider: 'local',
        url: '',
      },
    },
  })

  const screenshots = useWatch({ control, name: 'screenshots' })
  const hostnameStrategy = useWatch({
    control,
    name: 'public_hostnames.strategy',
  })

  useEffect(() => {
    setBreadcrumbs([{ label: 'Settings' }])
  }, [setBreadcrumbs])

  usePageTitle('Settings')

  useEffect(() => {
    if (settings) {
      reset({
        external_url: settings.external_url || '',
        internal_url: settings.internal_url || '',
        preview_domain: settings.preview_domain || 'localho.st',
        public_hostnames: {
          strategy: settings.public_hostnames?.strategy || 'standard',
          environment_template:
            settings.public_hostnames?.environment_template || '',
          service_template: settings.public_hostnames?.service_template || '',
          deployment_template:
            settings.public_hostnames?.deployment_template || '',
        },
        screenshots: settings.screenshots || {
          enabled: false,
          provider: 'local',
          url: '',
        },
      })
    }
  }, [settings, reset])

  const onSubmit = async (data: SettingsFormData) => {
    try {
      const normalized: SettingsFormData = {
        ...data,
        public_hostnames: {
          strategy: data.public_hostnames?.strategy || 'standard',
          environment_template: optionalTemplate(
            data.public_hostnames?.environment_template
          ),
          service_template: optionalTemplate(
            data.public_hostnames?.service_template
          ),
          deployment_template: optionalTemplate(
            data.public_hostnames?.deployment_template
          ),
        },
      }
      await updateSettings.mutateAsync(normalized)
      reset({
        ...normalized,
        public_hostnames: {
          ...normalized.public_hostnames,
          environment_template:
            normalized.public_hostnames.environment_template || '',
          service_template: normalized.public_hostnames.service_template || '',
          deployment_template:
            normalized.public_hostnames.deployment_template || '',
        },
      })
      toast.success('Settings saved successfully')
    } catch (err: any) {
      const detail =
        err?.body?.detail ||
        err?.message ||
        'Failed to save settings. Please try again.'
      toast.error(detail)
    }
  }

  if (isLoading) {
    return (
      <div className="flex items-center justify-center min-h-[400px]">
        <Loader2 className="h-8 w-8 animate-spin" />
      </div>
    )
  }

  if (error) {
    return (
      <Alert variant="destructive">
        <AlertCircle className="h-4 w-4" />
        <AlertTitle>Error</AlertTitle>
        <AlertDescription>
          Failed to load settings. Please try again later.
        </AlertDescription>
      </Alert>
    )
  }

  return (
    <form onSubmit={handleSubmit(onSubmit)} className="space-y-6">
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Link className="h-5 w-5" />
            External URL
          </CardTitle>
          <CardDescription>
            Set the external URL for your platform
          </CardDescription>
        </CardHeader>
        <CardContent>
          <div className="space-y-2">
            <Label htmlFor="external-url">External URL</Label>
            <Input
              id="external-url"
              type="url"
              placeholder="https://your-domain.com"
              {...register('external_url', {
                validate: (value) => {
                  if (!value) return true // optional
                  const trimmed = value.trim()
                  if (!trimmed) return true
                  if (
                    !trimmed.startsWith('http://') &&
                    !trimmed.startsWith('https://')
                  )
                    return 'Must start with http:// or https://'
                  if (trimmed.includes('#') || trimmed.includes('?'))
                    return 'Must not contain # or ? characters'
                  try {
                    new URL(trimmed)
                  } catch {
                    return 'Must be a valid URL'
                  }
                  return true
                },
              })}
            />
            {errors.external_url && (
              <p className="text-sm text-destructive">
                {errors.external_url.message}
              </p>
            )}
            <p className="text-sm text-muted-foreground">
              Used for OAuth callbacks, webhooks, and external integrations
            </p>
          </div>

          <div className="space-y-2 pt-4">
            <Label htmlFor="internal-url">Internal URL</Label>
            <Input
              id="internal-url"
              type="url"
              placeholder="http://host.docker.internal:8080"
              {...register('internal_url', {
                validate: (value) => {
                  if (!value) return true // optional — falls back to default
                  const trimmed = value.trim()
                  if (!trimmed) return true
                  if (
                    !trimmed.startsWith('http://') &&
                    !trimmed.startsWith('https://')
                  )
                    return 'Must start with http:// or https://'
                  if (trimmed.includes('#') || trimmed.includes('?'))
                    return 'Must not contain # or ? characters'
                  try {
                    new URL(trimmed)
                  } catch {
                    return 'Must be a valid URL'
                  }
                  return true
                },
              })}
            />
            {errors.internal_url && (
              <p className="text-sm text-destructive">
                {errors.internal_url.message}
              </p>
            )}
            <p className="text-sm text-muted-foreground">
              How service containers reach the Temps API from inside the Docker
              network (OTLP metrics ingest, agent callbacks). Leave blank to use{' '}
              <code className="font-mono text-xs">
                http://host.docker.internal:&lt;proxy-port&gt;
              </code>
              .
            </p>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Globe className="h-5 w-5" />
            Preview Domain
          </CardTitle>
          <CardDescription>
            Configure the domain used for deployment previews
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="space-y-2">
            <Label htmlFor="preview-domain">Preview Domain</Label>
            <Input
              id="preview-domain"
              type="text"
              placeholder="localho.st"
              {...register('preview_domain')}
            />
            <p className="text-sm text-muted-foreground">
              Deployments will be accessible at subdomain.
              {settings?.preview_domain || 'localho.st'}
            </p>
          </div>

          <div className="space-y-2">
            <Label htmlFor="hostname-strategy">Hostname Strategy</Label>
            <Select
              value={hostnameStrategy || 'standard'}
              onValueChange={(value: 'standard' | 'flat') =>
                setValue('public_hostnames.strategy', value, {
                  shouldDirty: true,
                })
              }
            >
              <SelectTrigger id="hostname-strategy">
                <SelectValue placeholder="Select hostname strategy" />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="standard">Standard</SelectItem>
                <SelectItem value="flat">Flat wildcard</SelectItem>
              </SelectContent>
            </Select>
            <p className="text-sm text-muted-foreground">
              Flat wildcard keeps generated service hostnames one label under
              the preview domain for providers such as Cloudflare Universal SSL.
            </p>
          </div>

          <div className="grid gap-4 md:grid-cols-3">
            <div className="space-y-2">
              <Label htmlFor="environment-template">Environment Template</Label>
              <Input
                id="environment-template"
                type="text"
                placeholder="{environment}.{base_domain}"
                {...register('public_hostnames.environment_template')}
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="service-template">Service Template</Label>
              <Input
                id="service-template"
                type="text"
                placeholder="{environment}-{service}.{base_domain}"
                {...register('public_hostnames.service_template')}
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="deployment-template">Deployment Template</Label>
              <Input
                id="deployment-template"
                type="text"
                placeholder="{deployment}.{base_domain}"
                {...register('public_hostnames.deployment_template')}
              />
            </div>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Image className="h-5 w-5" />
            Screenshots
          </CardTitle>
          <CardDescription>
            Configure screenshot generation for deployments
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="flex items-center justify-between">
            <div className="space-y-0.5">
              <Label htmlFor="screenshots-enabled">Enable Screenshots</Label>
              <p className="text-sm text-muted-foreground">
                Generate screenshots of deployments for previews
              </p>
            </div>
            <Switch
              id="screenshots-enabled"
              checked={screenshots?.enabled}
              onCheckedChange={(checked) =>
                setValue('screenshots.enabled', checked, {
                  shouldDirty: true,
                })
              }
            />
          </div>

          {screenshots?.enabled && (
            <>
              <div className="space-y-2">
                <Label htmlFor="screenshot-provider">Provider</Label>
                <Select
                  value={screenshots?.provider}
                  onValueChange={(value: 'local' | 'external') =>
                    setValue('screenshots.provider', value, {
                      shouldDirty: true,
                    })
                  }
                >
                  <SelectTrigger id="screenshot-provider">
                    <SelectValue placeholder="Select provider" />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="local">
                      Local Screenshot Service
                    </SelectItem>
                    <SelectItem value="external">
                      External Screenshot API
                    </SelectItem>
                  </SelectContent>
                </Select>
              </div>

              {screenshots.provider === 'external' && (
                <div className="space-y-2">
                  <Label htmlFor="screenshot-url">Screenshot API URL</Label>
                  <Input
                    id="screenshot-url"
                    type="url"
                    placeholder="https://<your-domain>/api/screenshot?url={url}&width=1920&height=1080"
                    {...register('screenshots.url')}
                  />
                  <p className="text-sm text-muted-foreground">
                    Configure your API endpoint with{' '}
                    <code className="px-1 py-0.5 bg-muted rounded text-xs">
                      {'{url}'}
                    </code>{' '}
                    placeholder.
                  </p>
                </div>
              )}
            </>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <RefreshCw className="h-5 w-5" />
            Route Table
          </CardTitle>
          <CardDescription>
            Manually refresh the proxy route table from the database. Use this
            if routes appear out of sync after deployments or configuration
            changes.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <Button
            type="button"
            variant="outline"
            disabled={isRefreshingRoutes}
            onClick={async () => {
              setIsRefreshingRoutes(true)
              try {
                const response = await client.post({
                  url: '/settings/routes/refresh',
                  security: [{ scheme: 'bearer', type: 'http' }],
                })
                const data = response.data as
                  | { route_count: number; message: string }
                  | undefined
                toast.success(
                  data?.message || 'Route table refreshed successfully'
                )
              } catch {
                toast.error('Failed to refresh route table')
              } finally {
                setIsRefreshingRoutes(false)
              }
            }}
          >
            {isRefreshingRoutes ? (
              <>
                <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                Refreshing...
              </>
            ) : (
              <>
                <RefreshCw className="mr-2 h-4 w-4" />
                Refresh Routes
              </>
            )}
          </Button>
        </CardContent>
      </Card>

      {isDirty && (
        <div className="sticky bottom-0 bg-background border-t pt-4 pb-2">
          <div className="flex flex-col gap-2 sm:flex-row sm:justify-between sm:items-center">
            <p className="text-sm text-muted-foreground">
              You have unsaved changes
            </p>
            <Button
              type="submit"
              disabled={isSubmitting}
              className="w-full sm:w-auto"
            >
              {isSubmitting ? (
                <>
                  <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                  Saving...
                </>
              ) : (
                <>
                  <Save className="mr-2 h-4 w-4" />
                  Save Changes
                </>
              )}
            </Button>
          </div>
        </div>
      )}
    </form>
  )
}
