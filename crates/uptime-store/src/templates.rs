//! Incident templates: reusable wording for declaring and updating incidents.

use uptime_domain::IncidentTemplate;

use crate::{Result, Store, StoreError, convert, models::IncidentTemplateRecord};

/// A saved template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template {
    pub id: i64,
    pub spec: IncidentTemplate,
}

impl Store {
    pub async fn create_template(&self, spec: &IncidentTemplate) -> Result<Template> {
        let spec = spec.clone().normalized()?;
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        if IncidentTemplateRecord::filter_by_name(spec.name.as_str())
            .first()
            .exec(&mut tx)
            .await?
            .is_some()
        {
            return Err(StoreError::DuplicateTemplate(spec.name));
        }
        let record = toasty::create!(IncidentTemplateRecord {
            name: spec.name.as_str(),
            title: spec.title.as_str(),
            impact: spec.impact.as_str(),
            status: spec.status.as_str(),
            body: spec.body.as_str(),
        })
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(Template {
            id: record.id,
            spec,
        })
    }

    pub async fn update_template(&self, id: i64, spec: &IncidentTemplate) -> Result<Template> {
        let spec = spec.clone().normalized()?;
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        if IncidentTemplateRecord::filter_by_id(id)
            .first()
            .exec(&mut tx)
            .await?
            .is_none()
        {
            return Err(StoreError::TemplateNotFound(id));
        }
        if IncidentTemplateRecord::filter_by_name(spec.name.as_str())
            .first()
            .exec(&mut tx)
            .await?
            .is_some_and(|other| other.id != id)
        {
            return Err(StoreError::DuplicateTemplate(spec.name));
        }
        IncidentTemplateRecord::update_by_id(id)
            .name(spec.name.as_str())
            .title(spec.title.as_str())
            .impact(spec.impact.as_str())
            .status(spec.status.as_str())
            .body(spec.body.as_str())
            .exec(&mut tx)
            .await?;
        tx.commit().await?;
        Ok(Template { id, spec })
    }

    pub async fn delete_template(&self, id: i64) -> Result<bool> {
        let deleted = toasty::sql::statement(r#"DELETE FROM "incident_templates" WHERE "id" = $1"#)
            .bind(id)
            .exec(&mut self.db())
            .await?;
        Ok(deleted > 0)
    }

    pub async fn template(&self, id: i64) -> Result<Option<Template>> {
        IncidentTemplateRecord::filter_by_id(id)
            .first()
            .exec(&mut self.db())
            .await?
            .map(template)
            .transpose()
    }

    pub async fn template_by_name(&self, name: &str) -> Result<Option<Template>> {
        IncidentTemplateRecord::filter_by_name(name.trim())
            .first()
            .exec(&mut self.db())
            .await?
            .map(template)
            .transpose()
    }

    /// Every template, by name.
    pub async fn list_templates(&self) -> Result<Vec<Template>> {
        let mut templates = IncidentTemplateRecord::all()
            .exec(&mut self.db())
            .await?
            .into_iter()
            .map(template)
            .collect::<Result<Vec<_>>>()?;
        templates.sort_by(|a, b| a.spec.name.cmp(&b.spec.name));
        Ok(templates)
    }
}

fn template(record: IncidentTemplateRecord) -> Result<Template> {
    Ok(Template {
        id: record.id,
        spec: IncidentTemplate {
            name: record.name,
            title: record.title,
            impact: convert::parse("incident_templates.impact", &record.impact)?,
            status: convert::parse("incident_templates.status", &record.status)?,
            body: record.body,
        },
    })
}
