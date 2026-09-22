use prost::Message;

use crate::build::attribute::Discriminator;
use crate::build::BuildLanguage;
use crate::build::RuleDefinition;
use crate::builtin::Callable;
use crate::builtin::Param;
use crate::builtin::Value;

pub fn decode_rules(build_language_output: &[u8]) -> anyhow::Result<BuildLanguage> {
    Ok(BuildLanguage::decode(build_language_output)?)
}

/// Project a native rule into the callable inventory used by editor declarations.
pub fn rule_value(rule: &RuleDefinition) -> Value {
    Value {
        name: rule.name.clone(),
        doc: rule.documentation.clone().unwrap_or_default(),
        callable: Some(Callable {
            param: rule
                .attribute
                .iter()
                .filter(|attribute| !attribute.name.starts_with(['$', ':']))
                .map(|attribute| Param {
                    name: attribute.name.clone(),
                    r#type: attribute_type_string_from_discriminator(attribute.r#type()),
                    doc: attribute.documentation().to_owned(),
                    is_mandatory: attribute.mandatory(),
                    ..Default::default()
                })
                .collect(),
            return_type: "None".to_owned(),
        }),
        ..Default::default()
    }
}

pub fn attribute_type_string_from_discriminator(value: Discriminator) -> String {
    use Discriminator::*;

    match value {
        Integer | Tristate => "int",
        String | License => "string",
        Label => "Label",
        StringList | DistributionSet => "List of strings",
        LabelList => "List of Labels",
        Boolean => "boolean",
        IntegerList => "List of ints",
        LabelListDict => "Dict of Labels",
        StringDict => "Dict of strings",
        StringListDict => "Dictionary: string -> list of strings",
        LabelKeyedStringDict => "Dictionary: Label -> string",
        _ => "Unknown",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::rule_value;
    use crate::build::attribute::Discriminator;
    use crate::build::AttributeDefinition;
    use crate::build::RuleDefinition;

    #[test]
    fn rule_attributes_preserve_requiredness() {
        let rule = RuleDefinition {
            name: "example".to_owned(),
            attribute: [None, Some(false), Some(true)]
                .into_iter()
                .map(|mandatory| AttributeDefinition {
                    name: "value".to_owned(),
                    r#type: Discriminator::String as i32,
                    mandatory,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let crate::builtin::Value {
            callable: Some(callable),
            ..
        } = rule_value(&rule)
        else {
            panic!("expected a callable");
        };
        assert_eq!(
            callable
                .param
                .iter()
                .map(|parameter| parameter.is_mandatory)
                .collect::<Vec<_>>(),
            [false, false, true]
        );
    }
}
