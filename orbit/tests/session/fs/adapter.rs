use super::*;

/// Без `DefaultPermissions` ядро не проверяет права в `$HOME` runner (`0700`, владелец — агент): session от root без
/// `CAP_DAC_OVERRIDE` не создала бы там точку монтирования проекта, и агент не запустился бы. У проекта проверка нужна.
#[test]
fn kernel_permission_checks_apply_to_the_project_fs_only() {
    assert!(
        mount_options(Root::Project).contains(&MountOption::DefaultPermissions),
        "ФС проекта смонтирована без проверки прав ядром"
    );
    assert!(
        !mount_options(Root::Home).contains(&MountOption::DefaultPermissions),
        "ФС `$HOME` смонтирована с проверкой прав ядром: проект внутри закрытого `$HOME` не смонтируется"
    );
}
